// ARM aarch32 target support.

#![allow(non_camel_case_types)]

use static_assertions::assert_cfg;
assert_cfg!(all(target_arch = "arm", target_pointer_width = "32"));

mod arm;
pub use arm::*;

use capdl::*;
use capdl::CDL_CapType::*;
use capdl::CDL_ObjectType::*;
use crate::KataOsModel;

use sel4_sys::seL4_CapInitThreadCNode;
use sel4_sys::seL4_CapIRQControl;
use sel4_sys::seL4_CPtr;
use sel4_sys::seL4_Error;
use sel4_sys::seL4_IRQControl_GetTrigger;
use sel4_sys::seL4_LargePageBits;
use sel4_sys::seL4_ObjectType::*;
use sel4_sys::seL4_ObjectType;
use sel4_sys::seL4_PageBits;
use sel4_sys::seL4_PageTableIndexBits;
use sel4_sys::seL4_Result;
use sel4_sys::seL4_UserContext;
use sel4_sys::seL4_Word;
use sel4_sys::seL4_WordBits;

const CDL_PT_NUM_LEVELS: usize = 2;
// TOOD(sleffler): levels really should be 0 & 1, the names are vestiges of 64-bit support
const CDL_PT_LEVEL_3_IndexBits: usize = seL4_PageTableIndexBits;

fn MASK(pow2_bits: usize) -> usize { (1 << pow2_bits) - 1 }

pub fn get_frame_type(object_size: seL4_Word) -> seL4_ObjectType {
    match object_size {
        seL4_PageBits => seL4_ARM_SmallPageObject,
        seL4_LargePageBits => seL4_ARM_LargePageObject,
        _ => panic!("Unexpected frame size {}", object_size),
    }
}

pub fn create_irq_cap(irq: CDL_IRQ, obj: &CDL_Object, free_slot: seL4_CPtr) -> seL4_Result {
    assert_eq!(obj.r#type(), CDL_ARMInterrupt);
    // XXX seL4_IRQControl_GetTriggerCore for NUM_NODES > 1
    unsafe {
        seL4_IRQControl_GetTrigger(
            seL4_CapIRQControl,
            irq,
            obj.armirq_trigger(),
            /*root=*/ seL4_CapInitThreadCNode as usize,
            /*index=*/ free_slot,
            /*depth=*/ seL4_WordBits as u8,
        )
    }
}

pub fn get_user_context(cdl_tcb: &CDL_Object, sp: seL4_Word) -> *const seL4_UserContext {
    #[rustfmt::skip]
    static mut regs: seL4_UserContext = seL4_UserContext {
        pc: 0, sp: 0, cpsr: 0,
        r0:  0, r1:  0, r8:  0, r9:  0, r10: 0, r11: 0, r12: 0,
        r2:  0, r3:  0, r4:  0, r5:  0, r6:  0, r7:  0, r14: 0,
        tpidrurw: 0, tpidruro: 0,
    };

    assert_eq!(cdl_tcb.r#type(), CDL_TCB);

    unsafe {
        regs.pc = cdl_tcb.tcb_pc();
        regs.sp = sp; // NB: may be adjusted from cdl_tcb.tcb_sp()

        let argv = core::slice::from_raw_parts(cdl_tcb.tcb_init(), cdl_tcb.tcb_init_sz());
        regs.r0 = if argv.len() > 0 { argv[0] } else { 0 };
        regs.r1 = if argv.len() > 1 { argv[1] } else { 0 };
        regs.r2 = if argv.len() > 2 { argv[2] } else { 0 };
        regs.r3 = if argv.len() > 3 { argv[3] } else { 0 };

        //        trace!("Start {} with pc {:#x} sp {:#x} argv {:?}", cdl_tcb.name(),
        //               regs.pc, regs.sp, argv);

        &regs as *const seL4_UserContext
    }
}

impl<'a> KataOsModel<'a> {
    pub fn create_arch_object(
        &mut self,
        _obj: &CDL_Object,
        _id: CDL_ObjID,
        _free_slot: usize,
    ) -> Option<seL4_Error> {
        // CDL_SID objects with CONFIG_ARM_SMU?
        None
    }

    // TODO(sleffler): BLINDLY COPIED FROM RISCV, CONVERT

    pub fn init_vspace(&mut self, obj_id: CDL_ObjID) -> seL4_Result {
        self.init_level_2(obj_id, 0, obj_id)
    }

    fn init_level_3(
        &mut self,
        level_3_obj: CDL_ObjID,
        level_0_obj: CDL_ObjID,
        level_3_base: usize,
    ) -> seL4_Result {
        for slot in self.get_object(level_3_obj).slots_slice() {
            let frame_cap = &slot.cap;
            self.map_page_frame(
                frame_cap,
                level_0_obj,
                frame_cap.cap_rights().into(),
                level_3_base + (slot.slot << seL4_PageBits),
            )?;
        }
        Ok(())
    }

    fn init_level_2(
        &mut self,
        level_0_obj: CDL_ObjID,
        level_2_base: usize,
        level_2_obj: CDL_ObjID,
    ) -> seL4_Result {
        for slot in self.get_object(level_2_obj).slots_slice() {
            let base = level_2_base + (slot.slot << (CDL_PT_LEVEL_3_IndexBits + seL4_PageBits));
            let level_3_cap = &slot.cap;
            if level_3_cap.r#type() == CDL_FrameCap {
                self.map_page_frame(
                    level_3_cap,
                    level_0_obj,
                    level_3_cap.cap_rights().into(),
                    base,
                )?;
            } else {
                let level_3_obj = level_3_cap.obj_id;
                self.map_page_table(level_3_cap, level_0_obj, base)?;
                self.init_level_3(level_3_obj, level_0_obj, base)?;
            }
        }
        Ok(())
    }

    pub fn get_cdl_frame_pt(&self, pd: CDL_ObjID, vaddr: usize) -> Option<&'a CDL_Cap> {
        self.get_cdl_frame_pt_recurse(pd, vaddr, 2)
    }

    /**
     * Do a recursive traversal from the top to bottom of a page table structure to
     * get the cap for a particular page table object for a certain vaddr at a certain
     * level. The level variable treats level==CDL_PT_NUM_LEVELS as the root page table
     * object, and level 0 as the bottom level 4k frames.
     */
    fn get_cdl_frame_pt_recurse(
        &self,
        root: CDL_ObjID,
        vaddr: usize,
        level: usize,
    ) -> Option<&'a CDL_Cap> {
        fn PT_LEVEL_SLOT(vaddr: usize, level: usize) -> usize {
            (vaddr >> ((seL4_PageTableIndexBits * (level - 1)) + seL4_PageBits))
                & MASK(seL4_PageTableIndexBits)
        }

        let obj_id = if level < CDL_PT_NUM_LEVELS {
            self.get_cdl_frame_pt_recurse(root, vaddr, level + 1)?
                .obj_id
        } else {
            root
        };
        self.get_object(obj_id)
            .get_cap_at(PT_LEVEL_SLOT(vaddr, level))
    }
}
// Copyright 2021 Arm Limited (or its affiliates). All rights reserved.
// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod dist_regs;
pub mod icc_regs;
pub mod redist_regs;

pub use self::dist_regs::{get_dist_regs, read_ctlr, set_dist_regs, write_ctlr};
pub use self::icc_regs::{get_icc_regs, set_icc_regs};
pub use self::redist_regs::{get_redist_regs, set_redist_regs};
use hypervisor::CpuState;
use std::any::Any;
use std::result;
use std::sync::Arc;

/// Errors thrown while setting up the GIC.
#[derive(Debug)]
pub enum Error {
    /// Error while calling KVM ioctl for setting up the global interrupt controller.
    CreateGic(hypervisor::HypervisorVmError),
    /// Error while setting device attributes for the GIC.
    SetDeviceAttribute(hypervisor::HypervisorDeviceError),
    /// Error while getting device attributes for the GIC.
    GetDeviceAttribute(hypervisor::HypervisorDeviceError),
}
type Result<T> = result::Result<T, Error>;

pub trait GicDevice: Send {
    /// Returns the hypervisor agnostic Device of the GIC device
    fn device(&self) -> &Arc<dyn hypervisor::Device>;

    /// Returns the hypervisor agnostic Device of the ITS device
    fn its_device(&self) -> Option<&Arc<dyn hypervisor::Device>>;

    /// Returns the fdt compatibility property of the device
    fn fdt_compatibility(&self) -> &str;

    /// Returns the maint_irq fdt property of the device
    fn fdt_maint_irq(&self) -> u32;

    /// Returns an array with GIC device properties
    fn device_properties(&self) -> [u64; 4];

    /// Returns the number of vCPUs this GIC handles
    fn vcpu_count(&self) -> u64;

    /// Returns whether the GIC device is MSI compatible or not
    fn msi_compatible(&self) -> bool;

    /// Returns the MSI compatibility property of the device
    fn msi_compatibility(&self) -> &str;

    /// Returns the MSI reg property of the device
    fn msi_properties(&self) -> [u64; 2];

    fn set_its_device(&mut self, its_device: Option<Arc<dyn hypervisor::Device>>);

    /// Get the values of GICR_TYPER for each vCPU.
    fn set_gicr_typers(&mut self, vcpu_states: &[CpuState]);

    /// Downcast the trait object to its concrete type.
    fn as_any_concrete_mut(&mut self) -> &mut dyn Any;
}

pub mod kvm {
    use super::Result;
    use crate::aarch64::gic::dist_regs::{get_dist_regs, read_ctlr, set_dist_regs, write_ctlr};
    use crate::aarch64::gic::icc_regs::{get_icc_regs, set_icc_regs};
    use crate::aarch64::gic::redist_regs::{
        construct_gicr_typers, get_redist_regs, set_redist_regs,
    };
    use crate::aarch64::gic::GicDevice;
    use crate::layout;
    use anyhow::anyhow;
    use hypervisor::kvm::kvm_bindings;
    use hypervisor::CpuState;
    use serde::{Deserialize, Serialize};
    use std::any::Any;
    use std::boxed::Box;
    use std::convert::TryInto;
    use std::sync::Arc;
    use vm_memory::Address;
    use vm_migration::{
        Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable,
    };

    const GITS_CTLR: u32 = 0x0000;
    const GITS_IIDR: u32 = 0x0004;
    const GITS_CBASER: u32 = 0x0080;
    const GITS_CWRITER: u32 = 0x0088;
    const GITS_CREADR: u32 = 0x0090;
    const GITS_BASER: u32 = 0x0100;

    /// Access an ITS device attribute.
    ///
    /// This is a helper function to get/set the ITS device attribute depending
    /// the bool parameter `set` provided.
    pub fn gicv3_its_attr_access(
        its_device: &Arc<dyn hypervisor::Device>,
        group: u32,
        attr: u32,
        val: &u64,
        set: bool,
    ) -> crate::aarch64::gic::Result<()> {
        let mut gicv3_its_attr = kvm_bindings::kvm_device_attr {
            group,
            attr: attr as u64,
            addr: val as *const u64 as u64,
            flags: 0,
        };
        if set {
            its_device
                .set_device_attr(&gicv3_its_attr)
                .map_err(crate::aarch64::gic::Error::SetDeviceAttribute)?;
        } else {
            its_device
                .get_device_attr(&mut gicv3_its_attr)
                .map_err(crate::aarch64::gic::Error::GetDeviceAttribute)?;
        }
        Ok(())
    }

    /// Function that saves/restores ITS tables into guest RAM.
    ///
    /// The tables get flushed to guest RAM whenever the VM gets stopped.
    pub fn gicv3_its_tables_access(
        its_device: &Arc<dyn hypervisor::Device>,
        save: bool,
    ) -> crate::aarch64::gic::Result<()> {
        let attr = if save {
            u64::from(kvm_bindings::KVM_DEV_ARM_ITS_SAVE_TABLES)
        } else {
            u64::from(kvm_bindings::KVM_DEV_ARM_ITS_RESTORE_TABLES)
        };

        let init_gic_attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr,
            addr: 0,
            flags: 0,
        };
        its_device
            .set_device_attr(&init_gic_attr)
            .map_err(crate::aarch64::gic::Error::SetDeviceAttribute)
    }

    pub struct KvmGicV3Its {
        /// The hypervisor agnostic device for the GicV3
        device: Arc<dyn hypervisor::Device>,

        /// The hypervisor agnostic device for the Its device
        its_device: Option<Arc<dyn hypervisor::Device>>,

        /// Vector holding values of GICR_TYPER for each vCPU
        gicr_typers: Vec<u64>,

        /// GIC distributor address
        dist_addr: u64,

        /// GIC distributor size
        dist_size: u64,

        /// GIC distributors address
        redists_addr: u64,

        /// GIC distributors size
        redists_size: u64,

        /// GIC MSI address
        msi_addr: u64,

        /// GIC MSI size
        msi_size: u64,

        /// Number of CPUs handled by the device
        vcpu_count: u64,
    }

    #[derive(Clone, Default, Serialize, Deserialize)]
    pub struct Gicv3ItsState {
        dist: Vec<u32>,
        rdist: Vec<u32>,
        icc: Vec<u32>,
        // special register that enables interrupts and affinity routing
        gicd_ctlr: u32,
        its_ctlr: u64,
        its_iidr: u64,
        its_cbaser: u64,
        its_cwriter: u64,
        its_creadr: u64,
        its_baser: [u64; 8],
    }

    impl KvmGicV3Its {
        /// Device trees specific constants
        pub const ARCH_GIC_V3_MAINT_IRQ: u32 = 9;

        /// Save the state of GICv3ITS.
        fn state(&self, gicr_typers: &[u64]) -> Result<Gicv3ItsState> {
            let gicd_ctlr = read_ctlr(self.device())?;

            let dist_state = get_dist_regs(self.device())?;

            let rdist_state = get_redist_regs(self.device(), gicr_typers)?;

            let icc_state = get_icc_regs(self.device(), gicr_typers)?;

            let its_baser_state: [u64; 8] = [0; 8];
            for i in 0..8 {
                gicv3_its_attr_access(
                    self.its_device().unwrap(),
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                    GITS_BASER + i * 8,
                    &its_baser_state[i as usize],
                    false,
                )?;
            }

            let its_ctlr_state: u64 = 0;
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CTLR,
                &its_ctlr_state,
                false,
            )?;

            let its_cbaser_state: u64 = 0;
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CBASER,
                &its_cbaser_state,
                false,
            )?;

            let its_creadr_state: u64 = 0;
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CREADR,
                &its_creadr_state,
                false,
            )?;

            let its_cwriter_state: u64 = 0;
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CWRITER,
                &its_cwriter_state,
                false,
            )?;

            let its_iidr_state: u64 = 0;
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_IIDR,
                &its_iidr_state,
                false,
            )?;

            Ok(Gicv3ItsState {
                dist: dist_state,
                rdist: rdist_state,
                icc: icc_state,
                gicd_ctlr,
                its_ctlr: its_ctlr_state,
                its_iidr: its_iidr_state,
                its_cbaser: its_cbaser_state,
                its_cwriter: its_cwriter_state,
                its_creadr: its_creadr_state,
                its_baser: its_baser_state,
            })
        }

        /// Restore the state of GICv3ITS.
        fn set_state(&mut self, gicr_typers: &[u64], state: &Gicv3ItsState) -> Result<()> {
            write_ctlr(self.device(), state.gicd_ctlr)?;

            set_dist_regs(self.device(), &state.dist)?;

            set_redist_regs(self.device(), gicr_typers, &state.rdist)?;

            set_icc_regs(self.device(), gicr_typers, &state.icc)?;

            //Restore GICv3ITS registers
            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_IIDR,
                &state.its_iidr,
                true,
            )?;

            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CBASER,
                &state.its_cbaser,
                true,
            )?;

            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CREADR,
                &state.its_creadr,
                true,
            )?;

            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CWRITER,
                &state.its_cwriter,
                true,
            )?;

            for i in 0..8 {
                gicv3_its_attr_access(
                    self.its_device().unwrap(),
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                    GITS_BASER + i * 8,
                    &state.its_baser[i as usize],
                    true,
                )?;
            }

            // Restore ITS tables
            gicv3_its_tables_access(self.its_device().unwrap(), false)?;

            gicv3_its_attr_access(
                self.its_device().unwrap(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ITS_REGS,
                GITS_CTLR,
                &state.its_ctlr,
                true,
            )?;

            Ok(())
        }

        /// Returns the GIC version of the device
        fn version() -> u32 {
            kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3
        }

        /// Setup the device-specific attributes
        fn init_device_attributes(
            &mut self,
            vm: &Arc<dyn hypervisor::Vm>,
            nr_irqs: u32,
        ) -> crate::aarch64::gic::Result<()> {
            // GicV3 part attributes
            /* Setting up the distributor attribute.
             We are placing the GIC below 1GB so we need to substract the size of the distributor.
            */
            Self::set_device_attribute(
                self.device(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_DIST),
                &self.dist_addr as *const u64 as u64,
                0,
            )?;

            /* Setting up the redistributors' attribute.
            We are calculating here the start of the redistributors address. We have one per CPU.
            */
            Self::set_device_attribute(
                self.device(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST),
                &self.redists_addr as *const u64 as u64,
                0,
            )?;

            // ITS part attributes
            let mut its_device = kvm_bindings::kvm_create_device {
                type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_ITS,
                fd: 0,
                flags: 0,
            };

            let its_fd = vm
                .create_device(&mut its_device)
                .map_err(crate::aarch64::gic::Error::CreateGic)?;

            Self::set_device_attribute(
                &its_fd,
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
                u64::from(kvm_bindings::KVM_VGIC_ITS_ADDR_TYPE),
                &self.msi_addr as *const u64 as u64,
                0,
            )?;

            Self::set_device_attribute(
                &its_fd,
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
                u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
                0,
                0,
            )?;

            self.set_its_device(Some(its_fd));

            /* We need to tell the kernel how many irqs to support with this vgic.
             * See the `layout` module for details.
             */
            let nr_irqs_ptr = &nr_irqs as *const u32;
            Self::set_device_attribute(
                self.device(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
                0,
                nr_irqs_ptr as u64,
                0,
            )?;

            /* Finalize the GIC.
             * See https://code.woboq.org/linux/linux/virt/kvm/arm/vgic/vgic-kvm-device.c.html#211.
             */
            Self::set_device_attribute(
                self.device(),
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
                u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
                0,
                0,
            )?;

            Ok(())
        }

        /// Create a KVM Vgic device
        fn create_device(vm: &Arc<dyn hypervisor::Vm>) -> Result<Arc<dyn hypervisor::Device>> {
            let mut gic_device = kvm_bindings::kvm_create_device {
                type_: Self::version(),
                fd: 0,
                flags: 0,
            };

            vm.create_device(&mut gic_device)
                .map_err(super::Error::CreateGic)
        }

        /// Set a GIC device attribute
        fn set_device_attribute(
            device: &Arc<dyn hypervisor::Device>,
            group: u32,
            attr: u64,
            addr: u64,
            flags: u32,
        ) -> Result<()> {
            let attr = kvm_bindings::kvm_device_attr {
                flags,
                group,
                attr,
                addr,
            };
            device
                .set_device_attr(&attr)
                .map_err(super::Error::SetDeviceAttribute)?;

            Ok(())
        }

        /// Function that saves RDIST pending tables into guest RAM.
        ///
        /// The tables get flushed to guest RAM whenever the VM gets stopped.
        pub fn save_pending_tables(vgic: &Arc<dyn hypervisor::Device>) -> Result<()> {
            let init_gic_attr = kvm_bindings::kvm_device_attr {
                group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
                attr: u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES),
                addr: 0,
                flags: 0,
            };
            vgic.set_device_attr(&init_gic_attr)
                .map_err(super::Error::SetDeviceAttribute)
        }

        /// Method to initialize the GIC device
        #[allow(clippy::new_ret_no_self)]
        fn new(
            vm: &Arc<dyn hypervisor::Vm>,
            vcpu_count: u64,
            dist_addr: u64,
            dist_size: u64,
            redist_size: u64,
            msi_size: u64,
            nr_irqs: u32,
        ) -> Result<Box<dyn GicDevice>> {
            let vgic = Self::create_device(vm)?;
            let redists_size: u64 = redist_size * vcpu_count;
            let redists_addr: u64 = dist_addr - redists_size;
            let msi_addr: u64 = redists_addr - msi_size;

            let mut gic_device = Box::new(KvmGicV3Its {
                device: vgic,
                its_device: None,
                gicr_typers: vec![0; vcpu_count.try_into().unwrap()],
                dist_addr,
                dist_size,
                redists_addr,
                redists_size,
                msi_addr,
                msi_size,
                vcpu_count,
            });

            gic_device.init_device_attributes(vm, nr_irqs)?;

            Ok(gic_device)
        }
    }

    /// Create a GICv3-ITS device.
    ///
    pub fn create_gic(vm: &Arc<dyn hypervisor::Vm>, vcpu_count: u64) -> Result<Box<dyn GicDevice>> {
        debug!("creating a GICv3-ITS");
        KvmGicV3Its::new(
            vm,
            vcpu_count,
            layout::GIC_V3_DIST_START.raw_value(),
            layout::GIC_V3_DIST_SIZE,
            layout::GIC_V3_REDIST_SIZE,
            layout::GIC_V3_ITS_SIZE,
            layout::IRQ_NUM,
        )
    }

    impl GicDevice for KvmGicV3Its {
        fn device(&self) -> &Arc<dyn hypervisor::Device> {
            &self.device
        }

        fn its_device(&self) -> Option<&Arc<dyn hypervisor::Device>> {
            self.its_device.as_ref()
        }

        fn fdt_compatibility(&self) -> &str {
            "arm,gic-v3"
        }

        fn msi_compatible(&self) -> bool {
            true
        }

        fn msi_compatibility(&self) -> &str {
            "arm,gic-v3-its"
        }

        fn fdt_maint_irq(&self) -> u32 {
            KvmGicV3Its::ARCH_GIC_V3_MAINT_IRQ
        }

        fn vcpu_count(&self) -> u64 {
            self.vcpu_count
        }

        fn device_properties(&self) -> [u64; 4] {
            [
                self.dist_addr,
                self.dist_size,
                self.redists_addr,
                self.redists_size,
            ]
        }

        fn msi_properties(&self) -> [u64; 2] {
            [self.msi_addr, self.msi_size]
        }

        fn set_its_device(&mut self, its_device: Option<Arc<dyn hypervisor::Device>>) {
            self.its_device = its_device;
        }

        fn set_gicr_typers(&mut self, vcpu_states: &[CpuState]) {
            let gicr_typers = construct_gicr_typers(vcpu_states);
            self.gicr_typers = gicr_typers;
        }

        fn as_any_concrete_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    pub const GIC_V3_ITS_SNAPSHOT_ID: &str = "gic-v3-its";
    impl Snapshottable for KvmGicV3Its {
        fn id(&self) -> String {
            GIC_V3_ITS_SNAPSHOT_ID.to_string()
        }

        fn snapshot(&mut self) -> std::result::Result<Snapshot, MigratableError> {
            let gicr_typers = self.gicr_typers.clone();
            Snapshot::new_from_state(&self.id(), &self.state(&gicr_typers).unwrap())
        }

        fn restore(&mut self, snapshot: Snapshot) -> std::result::Result<(), MigratableError> {
            let gicr_typers = self.gicr_typers.clone();
            self.set_state(&gicr_typers, &snapshot.to_state(&self.id())?)
                .map_err(|e| {
                    MigratableError::Restore(anyhow!("Could not restore GICv3ITS state {:?}", e))
                })
        }
    }

    impl Pausable for KvmGicV3Its {
        fn pause(&mut self) -> std::result::Result<(), MigratableError> {
            // Flush redistributors pending tables to guest RAM.
            KvmGicV3Its::save_pending_tables(self.device()).map_err(|e| {
                MigratableError::Pause(anyhow!(
                    "Could not save GICv3ITS GIC pending tables {:?}",
                    e
                ))
            })?;
            // Flush ITS tables to guest RAM.
            gicv3_its_tables_access(self.its_device().unwrap(), true).map_err(|e| {
                MigratableError::Pause(anyhow!("Could not save GICv3ITS ITS tables {:?}", e))
            })?;

            Ok(())
        }
    }
    impl Transportable for KvmGicV3Its {}
    impl Migratable for KvmGicV3Its {}
}

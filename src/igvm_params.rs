// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

extern crate alloc;

use crate::acpi::tables::ACPICPUInfo;
use crate::address::{PhysAddr, VirtAddr};
use crate::cpu::efer::EFERFlags;
use crate::error::SvsmError;
use crate::error::SvsmError::Firmware;
use crate::fw_meta::SevFWMetaData;
use crate::mm::PAGE_SIZE;
use crate::utils::MemoryRegion;
use alloc::vec::Vec;
use cpuarch::vmsa::VMSA;

use bootlib::igvm_params::{IgvmGuestContext, IgvmParamBlock, IgvmParamPage};
use core::mem::size_of;
use igvm_defs::{IgvmEnvironmentInfo, MemoryMapEntryType, IGVM_VHS_MEMORY_MAP_ENTRY};

const IGVM_MEMORY_ENTRIES_PER_PAGE: usize = PAGE_SIZE / size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>();

const STAGE2_END_ADDR: usize = 0xA0000;

#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct IgvmMemoryMap {
    memory_map: [IGVM_VHS_MEMORY_MAP_ENTRY; IGVM_MEMORY_ENTRIES_PER_PAGE],
}

#[derive(Clone, Debug)]
pub struct IgvmParams<'a> {
    igvm_param_block: &'a IgvmParamBlock,
    igvm_param_page: &'a IgvmParamPage,
    igvm_memory_map: &'a IgvmMemoryMap,
    igvm_guest_context_address: VirtAddr,
}

impl IgvmParams<'_> {
    pub fn new(addr: VirtAddr) -> Self {
        let param_block = unsafe { &*addr.as_ptr::<IgvmParamBlock>() };
        let param_page_address = addr + param_block.param_page_offset.try_into().unwrap();
        let param_page = unsafe { &*param_page_address.as_ptr::<IgvmParamPage>() };
        let memory_map_address = addr + param_block.memory_map_offset.try_into().unwrap();
        let memory_map = unsafe { &*memory_map_address.as_ptr::<IgvmMemoryMap>() };
        let guest_context_address = if param_block.guest_context_offset != 0 {
            addr + param_block.guest_context_offset.try_into().unwrap()
        } else {
            VirtAddr::null()
        };

        Self {
            igvm_param_block: param_block,
            igvm_param_page: param_page,
            igvm_memory_map: memory_map,
            igvm_guest_context_address: guest_context_address,
        }
    }

    pub fn size(&self) -> usize {
        // Calculate the total size of the parameter area.  The
        // parameter area always begins at the kernel base
        // address.
        self.igvm_param_block.param_area_size.try_into().unwrap()
    }

    pub fn find_kernel_region(&self) -> Result<MemoryRegion<PhysAddr>, SvsmError> {
        let kernel_base = PhysAddr::from(self.igvm_param_block.kernel_base);
        let kernel_size: usize = self.igvm_param_block.kernel_size.try_into().unwrap();
        Ok(MemoryRegion::<PhysAddr>::new(kernel_base, kernel_size))
    }

    pub fn reserved_kernel_area_size(&self) -> usize {
        self.igvm_param_block
            .kernel_reserved_size
            .try_into()
            .unwrap()
    }

    pub fn page_state_change_required(&self) -> bool {
        let environment_info = IgvmEnvironmentInfo::from(self.igvm_param_page.environment_info);
        environment_info.memory_is_shared()
    }

    pub fn get_cpuid_page_address(&self) -> u64 {
        self.igvm_param_block.cpuid_page as u64
    }

    pub fn get_secrets_page_address(&self) -> u64 {
        self.igvm_param_block.secrets_page as u64
    }

    pub fn get_memory_regions(&self) -> Result<Vec<MemoryRegion<PhysAddr>>, SvsmError> {
        // Count the number of memory entries present.  They must be
        // non-overlapping and strictly increasing.
        let mut number_of_entries = 0;
        let mut next_page_number = 0;
        for i in 0..IGVM_MEMORY_ENTRIES_PER_PAGE {
            let entry = &self.igvm_memory_map.memory_map[i];
            if entry.number_of_pages == 0 {
                break;
            }
            if entry.starting_gpa_page_number < next_page_number {
                return Err(Firmware);
            }
            let next_supplied_page_number = entry.starting_gpa_page_number + entry.number_of_pages;
            if next_supplied_page_number < next_page_number {
                return Err(Firmware);
            }
            next_page_number = next_supplied_page_number;
            number_of_entries += 1;
        }

        // Now loop over the supplied entires and add a region for each
        // known type.
        let mut regions: Vec<MemoryRegion<PhysAddr>> = Vec::new();
        for i in 0..number_of_entries {
            let entry = &self.igvm_memory_map.memory_map[i];
            if entry.entry_type == MemoryMapEntryType::MEMORY {
                let starting_page: usize = entry.starting_gpa_page_number.try_into().unwrap();
                let number_of_pages: usize = entry.number_of_pages.try_into().unwrap();
                regions.push(MemoryRegion::new(
                    PhysAddr::new(starting_page * PAGE_SIZE),
                    number_of_pages * PAGE_SIZE,
                ));
            }
        }

        Ok(regions)
    }

    pub fn load_cpu_info(&self) -> Result<Vec<ACPICPUInfo>, SvsmError> {
        let mut cpus: Vec<ACPICPUInfo> = Vec::new();
        for i in 0..self.igvm_param_page.cpu_count {
            let cpu = ACPICPUInfo {
                apic_id: i,
                enabled: true,
            };
            cpus.push(cpu);
        }
        Ok(cpus)
    }

    pub fn should_launch_fw(&self) -> bool {
        self.igvm_param_block.firmware.size != 0
    }

    pub fn debug_serial_port(&self) -> u16 {
        self.igvm_param_block.debug_serial_port
    }

    pub fn get_fw_metadata(&self) -> Option<SevFWMetaData> {
        if !self.should_launch_fw() {
            None
        } else {
            let mut fw_meta = SevFWMetaData::new();

            if self.igvm_param_block.firmware.caa_page != 0 {
                fw_meta.caa_page = Some(PhysAddr::new(
                    self.igvm_param_block.firmware.caa_page.try_into().unwrap(),
                ));
            }

            if self.igvm_param_block.firmware.secrets_page != 0 {
                fw_meta.secrets_page = Some(PhysAddr::new(
                    self.igvm_param_block
                        .firmware
                        .secrets_page
                        .try_into()
                        .unwrap(),
                ));
            }

            if self.igvm_param_block.firmware.cpuid_page != 0 {
                fw_meta.cpuid_page = Some(PhysAddr::new(
                    self.igvm_param_block
                        .firmware
                        .cpuid_page
                        .try_into()
                        .unwrap(),
                ));
            }

            for preval in 0..self.igvm_param_block.firmware.prevalidated_count as usize {
                let base = PhysAddr::new(
                    self.igvm_param_block.firmware.prevalidated[preval]
                        .base
                        .try_into()
                        .unwrap(),
                );
                let size = self.igvm_param_block.firmware.prevalidated[preval].size as usize;
                fw_meta.add_valid_mem(base, size);
            }

            Some(fw_meta)
        }
    }

    pub fn get_fw_regions(&self) -> Vec<MemoryRegion<PhysAddr>> {
        assert!(self.should_launch_fw());

        let mut regions = Vec::new();

        if self.igvm_param_block.firmware.in_low_memory != 0 {
            // Add the stage 2 region to the firmware region list so
            // permissions can be granted to the guest VMPL for that range.
            regions.push(MemoryRegion::new(PhysAddr::new(0), STAGE2_END_ADDR));
        }

        regions.push(MemoryRegion::new(
            PhysAddr::new(self.igvm_param_block.firmware.start as usize),
            self.igvm_param_block.firmware.size as usize,
        ));

        regions
    }

    pub fn fw_in_low_memory(&self) -> bool {
        self.igvm_param_block.firmware.in_low_memory != 0
    }

    pub fn initialize_guest_vmsa(&self, vmsa: &mut VMSA) {
        if self.igvm_param_block.guest_context_offset != 0 {
            let guest_context =
                unsafe { &*self.igvm_guest_context_address.as_ptr::<IgvmGuestContext>() };

            // Copy the specified registers into the VMSA.
            vmsa.cr0 = guest_context.cr0;
            vmsa.cr3 = guest_context.cr3;
            vmsa.cr4 = guest_context.cr4;
            vmsa.efer = guest_context.efer;
            vmsa.rip = guest_context.rip;
            vmsa.rax = guest_context.rax;
            vmsa.rcx = guest_context.rcx;
            vmsa.rdx = guest_context.rdx;
            vmsa.rbx = guest_context.rbx;
            vmsa.rsp = guest_context.rsp;
            vmsa.rbp = guest_context.rbp;
            vmsa.rsi = guest_context.rsi;
            vmsa.rdi = guest_context.rdi;
            vmsa.r8 = guest_context.r8;
            vmsa.r9 = guest_context.r9;
            vmsa.r10 = guest_context.r10;
            vmsa.r11 = guest_context.r11;
            vmsa.r12 = guest_context.r12;
            vmsa.r13 = guest_context.r13;
            vmsa.r14 = guest_context.r14;
            vmsa.r15 = guest_context.r15;
            vmsa.gdt.base = guest_context.gdt_base;
            vmsa.gdt.limit = guest_context.gdt_limit;

            // If a non-zero code selector is specified, then set the code
            // segment attributes based on EFER.LMA.
            if guest_context.code_selector != 0 {
                vmsa.cs.selector = guest_context.code_selector;
                let efer_lma = EFERFlags::LMA;
                if (vmsa.efer & efer_lma.bits()) != 0 {
                    vmsa.cs.flags = 0xA9B;
                } else {
                    vmsa.cs.flags = 0xC9B;
                    vmsa.cs.limit = 0xFFFFFFFF;
                }
            }

            let efer_svme = EFERFlags::SVME;
            vmsa.efer &= !efer_svme.bits();

            // If a non-zero data selector is specified, then modify the data
            // segment attributes to be compatible with protected mode.
            if guest_context.data_selector != 0 {
                vmsa.ds.selector = guest_context.data_selector;
                vmsa.ds.flags = 0xA93;
                vmsa.ds.limit = 0xFFFFFFFF;
                vmsa.ss = vmsa.ds;
                vmsa.es = vmsa.ds;
                vmsa.fs = vmsa.ds;
                vmsa.gs = vmsa.ds;
            }

            // Configure vTOM if reqested.
            if self.igvm_param_block.vtom != 0 {
                vmsa.vtom = self.igvm_param_block.vtom;
                vmsa.sev_features |= 2; // VTOM feature
            }
        }
    }
}

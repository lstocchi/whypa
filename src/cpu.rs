use acpi_tables::{Aml, aml, sdt::Sdt};
use zerocopy_derive::{FromBytes, Immutable, IntoBytes};

use crate::memory::memory::GuestAddress;

const ACPI_X2APIC_PROCESSOR: u8 = 9;
const ACPI_APIC_PROCESSOR: u8 = 0;
const ACPI_APIC_IO: u8 = 1;
const ACPI_APIC_XRUPT_OVERRIDE: u8 = 2;

struct Cpu {
    id: u32,
}

impl Aml for Cpu {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        aml::Device::new(format!("CP{:02X}", self.id).as_str().into(), vec![
            &aml::Name::new("_HID".into(), &"ACPI0007"),
            &aml::Name::new("_UID".into(), &self.id),
            /*
            _STA return value:
            Bit [0] – Set if the device is present.
            Bit [1] – Set if the device is enabled and decoding its resources.
            Bit [2] – Set if the device should be shown in the UI.
            Bit [3] – Set if the device is functioning properly (cleared if device failed its diagnostics).
            Bit [4] – Set if the battery is present.
            Bits [31:5] – Reserved (must be cleared).
            */
            &aml::Method::new(
                "_STA".into(), 
                0, 
                false,
                // Mark CPU present see CSTA implementation
                vec![&aml::Return::new(&0xfu8)]),
        ]).to_aml_bytes(sink);
    }
}

#[repr(C, packed)]
#[derive(IntoBytes, Immutable, FromBytes)]
struct LocalX2Apic {
    pub r#type: u8,
    pub length: u8,
    pub _reserved: u16,
    pub apic_id: u32,
    pub flags: u32,
    pub processor_id: u32,
}

#[repr(C, packed)]
#[derive(IntoBytes, Immutable, FromBytes)]
struct LocalApic {
    pub r#type: u8,
    pub length: u8,
    pub apic_id: u8,
    pub flags: u32,
    pub processor_id: u8,
}

#[repr(C, packed)]
#[derive(Default, IntoBytes, Immutable, FromBytes)]
struct Ioapic {
    pub r#type: u8,
    pub length: u8,
    pub ioapic_id: u8,
    _reserved: u8,
    pub apic_address: u32,
    pub gsi_base: u32,
}

#[repr(C, packed)]
#[derive(Default, IntoBytes, Immutable, FromBytes)]
struct InterruptSourceOverride {
    pub r#type: u8,
    pub length: u8,
    pub bus: u8,
    pub source: u8,
    pub gsi: u32,
    pub flags: u16,
}

pub struct CpuManager {
    cpus: u32,
    acpi_address: Option<GuestAddress>,
}

impl CpuManager {
    pub fn new() -> Self {
        Self {
            acpi_address: None,
            cpus: 0,
        }
    }

    pub fn set_cpu_count(&mut self, count: u32) {
        self.cpus = count;
    }

    pub fn create_madt(&self) -> Sdt {
        let mut madt = Sdt::new(*b"APIC", 44, 5, *b"CLOUDH", *b"CHMADT  ", 1);
        
        madt.write(36, crate::memory::layout::APIC_START.0);

        madt.write(32, 0u32);

        for cpu in 0..self.cpus {
            /* let lapic = LocalX2Apic {
                r#type: ACPI_X2APIC_PROCESSOR,
                length: 16,
                processor_id: cpu,
                apic_id: cpu, // we use the same APIC ID as the processor ID
                flags: 1,
                _reserved: 0,
            }; */
            let lapic = LocalApic {
                r#type: ACPI_APIC_PROCESSOR,
                length: 8,
                processor_id: cpu as u8,
                apic_id: cpu as u8,
                flags: 1,       // Enabled
            };
            madt.append(lapic);
        }

        madt.append(Ioapic {
            r#type: ACPI_APIC_IO,
            length: 12,
            ioapic_id: 0,
            apic_address: crate::memory::layout::IOAPIC_START.0 as u32,
            gsi_base: 0,
            ..Default::default()
        });

        /* madt.append(InterruptSourceOverride {
            r#type: ACPI_APIC_XRUPT_OVERRIDE,
            length: 10,
            bus: 0,
            source: 4,
            gsi: 4,
            flags: 0,
        }); */

        madt.append(InterruptSourceOverride {
            r#type: ACPI_APIC_XRUPT_OVERRIDE,
            length: 10,
            bus: 0,    // ISA
            source: 0, // IRQ 0
            gsi: 2,    // MUST BE GSI 2
            flags: 0,
        });
        
        
        madt.update_checksum();
        madt
    }
}

impl Aml for CpuManager {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
      
        let mut cpu_devices = Vec::new();
        let mut cpu_data_inner: Vec<&dyn Aml> = Vec::new();
        
        for cpu_id in 0..self.cpus {
            cpu_devices.push(Cpu { id: cpu_id });
        }

        for cpu_device in cpu_devices.iter() {
            cpu_data_inner.push(cpu_device);
        }

        //aml::Device::new("\\_PR_".into(), cpu_data_inner).to_aml_bytes(sink);
        aml::Scope::new("\\_SB_".into(), cpu_data_inner).to_aml_bytes(sink);
    }
}
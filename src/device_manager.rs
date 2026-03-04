use std::sync::{Arc, Mutex};

use acpi_tables::{Aml, aml, sdt::GenericAddress};

use crate::{acpi::{AcpiPlatformAddresses, AcpiPmTimerDevice}, devices::bus::Bus, memory::{layout, memory::GuestAddress}};

pub struct DeviceManager {
    io_bus: Arc<Mutex<Bus>>,
    pci_segments: Vec<PciSegment>,
    serial_devices: Vec<SerialDevice>,
    virtio_mmio_devices: Vec<VirtioMmioDevice>,

    // Addresses for ACPI platform devices e.g. ACPI PM timer, sleep/reset registers
    acpi_platform_addresses: AcpiPlatformAddresses,
}

pub struct PciSegment {
    id: u16,

    mmio_config_address: u64,

    start_of_mem32_area: u64,
    end_of_mem32_area: u64,

    start_of_mem64_area: u64,
    end_of_mem64_area: u64,
}

impl PciSegment {
    pub fn get_id(&self) -> u16 {
        self.id
    }

    pub fn get_mmio_config_address(&self) -> u64 {
        self.mmio_config_address
    }

    pub fn get_start_of_mem32_area(&self) -> u64 {
        self.start_of_mem32_area
    }

    pub fn get_end_of_mem32_area(&self) -> u64 {
        self.end_of_mem32_area
    }

    pub fn get_start_of_mem64_area(&self) -> u64 {
        self.start_of_mem64_area
    }
}

impl Aml for PciSegment {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        let mut pci_dsdt_inner_data: Vec<&dyn Aml> = Vec::new();
        let hid = aml::Name::new("_HID".into(), &aml::EISAName::new("PNP0A08"));
        pci_dsdt_inner_data.push(&hid);
        let cid = aml::Name::new("_CID".into(), &aml::EISAName::new("PNP0A03"));
        pci_dsdt_inner_data.push(&cid);
        let seg = aml::Name::new("_SEG".into(), &self.id);
        pci_dsdt_inner_data.push(&seg);
        let uid = aml::Name::new("_UID".into(), &self.id);
        pci_dsdt_inner_data.push(&uid);
        let bbn = aml::Name::new("_BBN".into(), &aml::ZERO);
        pci_dsdt_inner_data.push(&bbn);

        let crs = aml::Name::new(
            "_CRS".into(),
            &aml::ResourceTemplate::new(vec![
                &aml::AddressSpace::new_bus_number(0x0u16, 0xffu16),
                &aml::IO::new(0xcf8, 0xcf8, 1, 0x8),
                &aml::Memory32Fixed::new(
                    true,
                    self.mmio_config_address as u32,
                    layout::PCI_MMIO_CONFIG_SIZE_PER_SEGMENT as u32,
                ),
                &aml::AddressSpace::new_memory(
                    aml::AddressSpaceCacheable::NotCacheable,
                    true,
                    self.start_of_mem32_area,
                    self.end_of_mem32_area,
                    None,
                ),
                &aml::AddressSpace::new_memory(
                    aml::AddressSpaceCacheable::NotCacheable,
                    true,
                    self.start_of_mem64_area,
                    self.end_of_mem64_area,
                    None,
                ),
            ]),
        );

        pci_dsdt_inner_data.push(&crs);

        aml::Device::new(
            format!("PC{:02X}", self.id).as_str().into(),
            pci_dsdt_inner_data,
        ).to_aml_bytes(sink);

    }
}

struct SerialDevice {
    name: String,
    uid: u8,
    irq: u32,
    port: u16,
}

impl Aml for SerialDevice {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        aml::Device::new(
            format!("_SB_.{}", self.name).as_str().into(), 
            vec![
                &aml::Name::new("_HID".into(), &aml::EISAName::new("PNP0501")),
                &aml::Name::new("_UID".into(), &self.uid),
                &aml::Name::new("_CRS".into(), &aml::ResourceTemplate::new(vec![
                    &aml::Interrupt::new(true, true, false, false, self.irq),
                    &aml::IO::new(self.port, self.port, 0x01, 0x8),
                ])),
            ],
        ).to_aml_bytes(sink);
    }
}

struct VirtioMmioDevice {
    name: String,
    uid: u8,
    base_address: u64,
    size: u64,
    irq: u32,
}

impl Aml for VirtioMmioDevice {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        // For virtio-mmio devices, we use _HID "LNRO0005" (Linux Root Device)
        // This tells Linux to probe this as a virtio-mmio device
        aml::Device::new(
            format!("_SB_.{}", self.name).as_str().into(),
            vec![
                &aml::Name::new("_HID".into(), &"LNRO0005"),
                //&aml::Name::new("_HID".into(), &"LNXP0567"),
                &aml::Name::new("_UID".into(), &self.uid),
                &aml::Name::new("_CRS".into(), &aml::ResourceTemplate::new(vec![
                    &aml::Memory32Fixed::new(
                        true, // ReadWrite
                        self.base_address as u32,
                        self.size as u32,
                    ),
                    &aml::Interrupt::new(true, true, false, false, self.irq),
                ])),
            ],
        ).to_aml_bytes(sink);
    }
}

impl DeviceManager {
    pub fn new(total_memory_gpa: GuestAddress) -> Self {
        Self {
            io_bus: Arc::new(Mutex::new(Bus::new())),
            pci_segments: vec![
                PciSegment { 
                    id: 0, 
                    mmio_config_address: layout::PCI_MMCONFIG_START.0, 
                    start_of_mem32_area: layout::MEM_32BIT_DEVICES_START.0, 
                    end_of_mem32_area: layout::MEM_32BIT_DEVICES_START.0 + layout::MEM_32BIT_DEVICES_SIZE - 1, 
                    start_of_mem64_area: total_memory_gpa.0 + 1, 
                    end_of_mem64_area: 0x3fff_ffff_ffff,
                }, // for local VM, 1 segment is enough
            ],
            serial_devices: vec![
                SerialDevice {
                    name: "COM1".to_string(),
                    uid: 1,
                    irq: 4,
                    port: 0x3f8,
                },
            ],
            virtio_mmio_devices: Vec::new(),
            acpi_platform_addresses: AcpiPlatformAddresses::new(),
        }
    }

    pub fn init(&mut self) {
        self.add_acpi_platform_device();
    }

    pub fn acpi_platform_addresses(&self) -> &AcpiPlatformAddresses {
        &self.acpi_platform_addresses
    }

    /// Get a reference to the IO bus for dispatching IO port accesses to registered devices
    pub fn io_bus(&self) -> &Arc<Mutex<Bus>> {
        &self.io_bus
    }

    pub fn pci_segments(&self) -> &[PciSegment] {
        &self.pci_segments
    }

    /// Register a virtio-mmio device for ACPI
    pub fn register_virtio_mmio(&mut self, name: String, uid: u8, base_address: u64, size: u64, irq: u32) {
        self.virtio_mmio_devices.push(VirtioMmioDevice {
            name,
            uid,
            base_address,
            size,
            irq,
        });
    }

    fn add_acpi_platform_device(&mut self) {
        let pm_timer_device = Arc::new(Mutex::new(AcpiPmTimerDevice::new()));

        let pm_timer_pio_address: u16 = 0x608;

            /* self.address_manager
                .allocator
                .lock()
                .unwrap()
                .allocate_io_addresses(Some(GuestAddress(pm_timer_pio_address.into())), 0x4, None)
                .ok_or(DeviceManagerError::AllocateIoPort)?; */

            let _ = self.io_bus
                .lock()
                .unwrap()
                .insert(pm_timer_device, pm_timer_pio_address.into(), 0x4)
                .map_err(|e| eprintln!("Error inserting PM timer device: {:?}", e));

            self.acpi_platform_addresses.pm_timer_address =
                Some(GenericAddress::io_port_address::<u32>(pm_timer_pio_address));
    }
}

impl Aml for DeviceManager {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        // 1. PCI Segments
        for segment in &self.pci_segments {
            segment.to_aml_bytes(sink);
        }

        // 2. Serial Device (COM1)
        for device in &self.serial_devices {
            device.to_aml_bytes(sink);
        }

        // 3. Virtio-MMIO Devices
        for device in &self.virtio_mmio_devices {
            device.to_aml_bytes(sink);
        }

        // 4. System Shutdown (_S5)
        aml::Name::new("_S5_".into(), &aml::Package::new(vec![&5u8])).to_aml_bytes(sink);
    }
}
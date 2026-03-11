use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use acpi_tables::{Aml, aml, sdt::GenericAddress};
use anyhow::Result;
use tracing::{debug, info};

use crate::acpi::{AcpiPlatformAddresses, AcpiPmTimerDevice};
use crate::devices::bus::{Bus, BusDevice};
use crate::devices::event::WindowsEvent;
use crate::devices::legacy::ioapic::{IoApic, IoApicMmioAdapter};
use crate::devices::legacy::irqchip::{IrqChip, IrqChipDevice};
use crate::devices::virtio::block::{Block, CacheType, ImageType, SyncMode};
use crate::devices::virtio::console::device::Console;
use crate::devices::virtio::mmio::MmioTransport;
use crate::devices::virtio::rng::Rng;
use crate::memory::{layout, memory::GuestAddress};
use crate::partition::Partition;

/// IRQ assignments for virtio devices.
const VIRTIO_BLOCK_IRQ: u32 = 20;
const VIRTIO_RNG_IRQ: u32 = 21;
const VIRTIO_CONSOLE_IRQ: u32 = 22;

// ===========================================================================
// Device registration – wires up all devices on the partition
// ===========================================================================

/// Register all devices on the partition: IOAPIC, PCI ECAM stub, and virtio
/// devices (block, rng, console).
pub fn register_devices(
    partition: &mut Partition,
    rootfs_path: &str,
    input_buffer: Arc<Mutex<VecDeque<u8>>>,
    input_event: Arc<WindowsEvent>,
) -> Result<()> {
    info!("Registering devices");

    let irqchip = setup_ioapic(partition)?;
    setup_pci_ecam(partition)?;
    setup_virtio_block(partition, &irqchip, rootfs_path)?;
    setup_virtio_rng(partition, &irqchip)?;
    setup_virtio_console(partition, &irqchip, input_buffer, input_event)?;

    info!("All devices registered");
    Ok(())
}

// ---------------------------------------------------------------------------
// IOAPIC
// ---------------------------------------------------------------------------

/// Create the software IOAPIC, register its MMIO region, and return the
/// [`IrqChip`] handle so it can be shared with virtio transports.
fn setup_ioapic(partition: &mut Partition) -> Result<IrqChip> {
    let ioapic = IoApic::new(partition.handle);
    let irqchip: IrqChip = Arc::new(Mutex::new(
        IrqChipDevice::new(Box::new(ioapic)),
    ));
    let mmio_adapter = IoApicMmioAdapter::new(irqchip.clone());

    partition.register_mmio_region(
        layout::IOAPIC_START.0,
        layout::IOAPIC_SIZE,
        "IOAPIC".to_string(),
        Some("ioapic".to_string()),
    )?;
    partition.register_mmio_handler(
        "ioapic".to_string(),
        Box::new(mmio_adapter),
    );

    debug!(base = format_args!("0x{:X}", layout::IOAPIC_START.0),
           size = format_args!("0x{:X}", layout::IOAPIC_SIZE),
           "IOAPIC registered");
    Ok(irqchip)
}

// ---------------------------------------------------------------------------
// PCI ECAM stub
// ---------------------------------------------------------------------------

/// PCI ECAM stub handler – returns `0xFFFF_FFFF` for all config-space reads
/// (standard PCI "no device present" response) and ignores writes.
struct PciEcamHandler;

impl BusDevice for PciEcamHandler {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        // "No device present" – fill with 0xFF
        data.fill(0xFF);
    }
    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {}
}

/// Register the PCI ECAM (memory-mapped config space) stub so that PCI
/// enumeration reads return "no device" instead of faulting.
fn setup_pci_ecam(partition: &mut Partition) -> Result<()> {
    partition.register_mmio_region(
        layout::PCI_MMCONFIG_START.0,
        layout::PCI_MMCONFIG_SIZE,
        "PCI ECAM".to_string(),
        Some("pci_ecam".to_string()),
    )?;
    partition.register_mmio_handler(
        "pci_ecam".to_string(),
        Box::new(PciEcamHandler),
    );

    debug!(base = format_args!("0x{:X}", layout::PCI_MMCONFIG_START.0),
           size = format_args!("0x{:X}", layout::PCI_MMCONFIG_SIZE),
           "PCI ECAM stub registered");
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtio block device
// ---------------------------------------------------------------------------

/// Register the virtio block device backed by `rootfs_path`.
fn setup_virtio_block(
    partition: &mut Partition,
    irqchip: &IrqChip,
    rootfs_path: &str,
) -> Result<()> {
    let base = layout::VIRTIO_MMIO_START.0;
    let size = layout::VIRTIO_MMIO_SIZE_PER_DEVICE;

    // Create a default disk image when one doesn't exist yet.
    if !std::fs::metadata(rootfs_path).is_ok() {
        info!(path = rootfs_path, "Creating 1 GiB disk image");
        let file = std::fs::File::create(rootfs_path)?;
        file.set_len(1024 * 1024 * 1024)?; // 1 GiB
    }

    let block_device = Arc::new(Mutex::new(Block::new(
        "vda1".to_string(),
        None,
        CacheType::Writeback,
        rootfs_path.to_string(),
        ImageType::Raw,
        false,
        false,
        SyncMode::Full,
    )?));

    let mut transport = MmioTransport::new(
        partition.memory_manager().clone(),
        irqchip.clone(),
        block_device,
    )?;
    transport.set_irq_line(VIRTIO_BLOCK_IRQ);

    partition.register_mmio_region(
        base,
        size,
        "Virtio Block Device".to_string(),
        Some("virtio_block".to_string()),
    )?;
    partition.register_mmio_handler("virtio_block".to_string(), Box::new(transport));
    partition.device_manager_mut().register_virtio_mmio(
        "VBLK".to_string(), 1, base, size, VIRTIO_BLOCK_IRQ,
    );

    debug!(base = format_args!("0x{:X}", base), irq = VIRTIO_BLOCK_IRQ,
           path = rootfs_path, "Virtio block device registered");
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtio RNG device
// ---------------------------------------------------------------------------

/// Register the virtio entropy (RNG) device.
fn setup_virtio_rng(partition: &mut Partition, irqchip: &IrqChip) -> Result<()> {
    let base = layout::VIRTIO_MMIO_START.0 + layout::VIRTIO_MMIO_SIZE_PER_DEVICE;
    let size = layout::VIRTIO_MMIO_SIZE_PER_DEVICE;

    let rng_device = Arc::new(Mutex::new(Rng::new()));

    let mut transport = MmioTransport::new(
        partition.memory_manager().clone(),
        irqchip.clone(),
        rng_device,
    )?;
    transport.set_irq_line(VIRTIO_RNG_IRQ);

    partition.register_mmio_region(
        base,
        size,
        "Virtio RNG Device".to_string(),
        Some("virtio_rng".to_string()),
    )?;
    partition.register_mmio_handler("virtio_rng".to_string(), Box::new(transport));
    partition.device_manager_mut().register_virtio_mmio(
        "VRNG".to_string(), 2, base, size, VIRTIO_RNG_IRQ,
    );

    debug!(base = format_args!("0x{:X}", base), irq = VIRTIO_RNG_IRQ,
           "Virtio RNG device registered");
    Ok(())
}

// ---------------------------------------------------------------------------
// Virtio console device
// ---------------------------------------------------------------------------

/// Register the virtio console device wired to the host's stdin/stdout.
fn setup_virtio_console(
    partition: &mut Partition,
    irqchip: &IrqChip,
    input_buffer: Arc<Mutex<VecDeque<u8>>>,
    input_event: Arc<WindowsEvent>,
) -> Result<()> {
    let base = layout::VIRTIO_MMIO_START.0 + 2 * layout::VIRTIO_MMIO_SIZE_PER_DEVICE;
    let size = layout::VIRTIO_MMIO_SIZE_PER_DEVICE;

    let console_device = Arc::new(Mutex::new(
        Console::new(200, 200, input_buffer, input_event),
    ));

    let mut transport = MmioTransport::new(
        partition.memory_manager().clone(),
        irqchip.clone(),
        console_device,
    )?;
    transport.set_irq_line(VIRTIO_CONSOLE_IRQ);

    partition.register_mmio_region(
        base,
        size,
        "Virtio Console Device".to_string(),
        Some("virtio_console".to_string()),
    )?;
    partition.register_mmio_handler("virtio_console".to_string(), Box::new(transport));
    partition.device_manager_mut().register_virtio_mmio(
        "VCON".to_string(), 3, base, size, VIRTIO_CONSOLE_IRQ,
    );

    debug!(base = format_args!("0x{:X}", base), irq = VIRTIO_CONSOLE_IRQ,
           "Virtio console device registered");
    Ok(())
}

// ===========================================================================
// DeviceManager – tracks devices for ACPI table generation & IO bus dispatch
// ===========================================================================

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
            serial_devices: vec![],
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

use std::io::Write;

use windows::Win32::System::{
    Hypervisor::*,
};

use anyhow::Result;

static mut DUMMY_RTC_TIME: u64 = 0;
static mut DUMMY_RTC_COUNTER: u64 = 0;

static mut REFRESH_BIT: u8 = 0;
static mut SERIAL_IER: u8 = 0; // Interrupt Enable Register - Port 0x3F9

pub struct Emulator {
    pub handle: *mut std::ffi::c_void,
}

impl Emulator {
    pub fn new() -> Result<Self> {
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: std::mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(Self::io_port_callback),
            WHvEmulatorMemoryCallback: Some(Self::memory_callback),
            WHvEmulatorGetVirtualProcessorRegisters: Some(Self::get_vp_registers_callback),
            WHvEmulatorSetVirtualProcessorRegisters: Some(Self::set_vp_registers_callback),
            WHvEmulatorTranslateGvaPage: Some(Self::translate_gva_page_callback),
        };

        let mut emulator_handle_raw: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe {
            let result = WHvEmulatorCreateEmulator(&callbacks, &mut emulator_handle_raw);
            if result.is_ok() && !emulator_handle_raw.is_null() {
                ////eprintln!("WHV Emulator created successfully");
            } else {
                return Err(anyhow::anyhow!("Failed to create WHV Emulator: {:?}", result));
            }
        }
    
        Ok(Emulator {
            handle: emulator_handle_raw,
        })        
    }

    unsafe extern "system" fn io_port_callback(
        context: *const std::ffi::c_void,
        io_access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
    ) -> windows::core::HRESULT {
        if io_access.is_null() {
            return windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0);
        }

        

        unsafe {
            let partition = &mut *(context as *mut crate::partition::Partition);
            let io_info = &mut *io_access;
            let port = io_info.Port;
            let direction = io_info.Direction; // 0 = IN (read), 1 = OUT (write)
            let access_size = io_info.AccessSize;

            ////eprintln!("IO Port: 0x{:X}, Direction: {}, Access Size: {}", port, direction, access_size);
            if direction == 1 {
                // OUT instruction - write data
                let data = io_info.Data;
                match port {
                    0x3F8 | 0x2F8 | 0x3E8 | 0x2E8 | 0x652 | 42 => {
                        ////eprintln!("Serial port write: 0x{:X}, size: {}, port: 0x{:X}", data, access_size, port);
                        // Serial ports - output character
                        let data_out = std::slice::from_raw_parts(
                            &data as *const u32 as *const u8,
                            access_size as usize,
                        );
                        let _ = std::io::stdout().write(data_out);
                        let _ = std::io::stdout().flush();
                    }
                    0x3F9 => {
                        SERIAL_IER = io_info.Data as u8;
                    }
                    0xCF8 => {
                        // MMIO port - write data
                        partition.pci_address_config = data;
                    }
                    _ => {
                        // Try the IO bus for registered devices
                        let io_bus = partition.device_manager().io_bus().clone();
                        let bus = io_bus.lock().unwrap();
                        let data_bytes = data.to_le_bytes();
                        let size = (access_size as usize).min(4);
                        bus.write(0, port as u64, &data_bytes[..size]);
                    }
                }
            } else {
                // IN instruction - read data
                match port {
                    0x3F9 => {
                        io_info.Data = SERIAL_IER as u32;
                    }
                    43 => {
                        // Specific port for testing
                        io_info.Data = 99;
                    }
                    0x40 | 0x43 => {
                        DUMMY_RTC_COUNTER = DUMMY_RTC_COUNTER.wrapping_add(1);
                        if DUMMY_RTC_COUNTER % 100 == 0 {
                            DUMMY_RTC_TIME = DUMMY_RTC_TIME.wrapping_add(1);
                        }
                        io_info.Data = DUMMY_RTC_TIME as u32;
                    }
                    0x61 => {
                        // It simulates a more stable "hardware-like" behavior by ensuring the bit stays in one state for a consistent number of reads, and then flips.
                        static mut INTERNAL_COUNTER: u64 = 0;
                        
                        INTERNAL_COUNTER += 1;
                        
                        // Bit 4: Refresh Toggle (the one Linux is likely polling)
                        // Bit 5: Speaker Output (sometimes checked as a secondary timer)
                        
                        // Let's toggle Bit 4 every 128 reads. This provides a "slow" enough
                        // pulse that the kernel's loop will definitely catch both 0 and 1.
                        let bit4 = ((INTERNAL_COUNTER >> 7) & 1) << 4;
                        
                        // Let's also toggle Bit 5 at a different rate just in case
                        let bit5 = ((INTERNAL_COUNTER >> 6) & 1) << 5;
                        
                        io_info.Data = (bit4 | bit5) as u32;
                        
                    }
                    0x70 | 0x71 => {
                        io_info.Data = 0;
                    }
                    0xCF8 => {
                        // MMIO port - read data
                        io_info.Data = partition.pci_address_config;
                    }
                    0xCFC => {
                        // MMIO port - read data
                        io_info.Data = 0xFFFFFFFF;
                        let bus = (partition.pci_address_config >> 16) & 0xFF;
                        let dev = (partition.pci_address_config >> 11) & 0x1F;
                        let function = (partition.pci_address_config  >> 8) & 0x7;
                        let reg = (partition.pci_address_config >> 2) & 0x3F;
                        ////eprintln!("PCI Scan: Kernel is checking Bus {}, Device {}, function {}, register {}. Returning 0xFFFFFFFF", bus, dev, function, reg);
                    }
                    0x3FD => {
                        // Bit 5 = Transmitter Holding Register Empty
                        // Bit 6 = Transmitter Empty
                        // We return 0x60 to tell the kernel: "I'm ready to receive characters!"
                        io_info.Data = 0x60;
                    }
                    _ => {
                        // Try the IO bus for registered devices (e.g. ACPI PM Timer at 0x608)
                        let io_bus = partition.device_manager().io_bus().clone();
                        let bus = io_bus.lock().unwrap();
                        let mut data = [0u8; 4];
                        let size = (access_size as usize).min(4);
                        if bus.read(0, port as u64, &mut data[..size]) {
                            io_info.Data = u32::from_le_bytes(data);
                        } else {
                            // Unknown port - return 0
                            io_info.Data = 0;
                        }
                    }
                }
            }
        }

        windows::core::HRESULT(0) // S_OK
    }

    pub fn try_io_emulation(
        &self,
        context: *const std::ffi::c_void,
        vp_context: *const WHV_VP_EXIT_CONTEXT,
        io_port_access_ctx: *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    ) -> Result<WHV_EMULATOR_STATUS> {
        if self.handle.is_null() {
            return Err(anyhow::anyhow!("Emulator handle is null"));
        }
        
        unsafe {
            WHvEmulatorTryIoEmulation(self.handle, context, vp_context, io_port_access_ctx)
                .map_err(|e| anyhow::anyhow!("WHvEmulatorTryIoEmulation failed: {:?}", e))
        }
    }

    pub fn try_mmio_emulation(
        &self,
        context: *const std::ffi::c_void,
        vp_context: *const WHV_VP_EXIT_CONTEXT,
        memory_access_ctx: *const WHV_MEMORY_ACCESS_CONTEXT,
    ) -> Result<WHV_EMULATOR_STATUS> {
        if self.handle.is_null() {
            return Err(anyhow::anyhow!("Emulator handle is null"));
        }
        
        unsafe {
            WHvEmulatorTryMmioEmulation(self.handle, context, vp_context, memory_access_ctx)
                .map_err(|e| anyhow::anyhow!("WHvEmulatorTryMmioEmulation failed: {:?}", e))
        }
    }

    unsafe extern "system" fn memory_callback(
        context: *const std::ffi::c_void,
        memory_access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
    ) -> windows::core::HRESULT {
        // This callback is called by the emulator when handling MMIO
        // We don't log here to avoid double-logging (already logged in handle_exit)
        if memory_access.is_null() {
            return windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0);
        }

        unsafe {
            let partition = &*(context as *const crate::partition::Partition);
            let mem_info = &mut *memory_access;
            
            let gpa = mem_info.GpaAddress;
            let direction = mem_info.Direction; // 0 = Read, 1 = Write
            let access_size = mem_info.AccessSize as u32;
            
            // Check if this is an MMIO region
            if let Some(mmio_region) = partition.memory_manager().find_mmio(gpa) {
                // Calculate offset within the MMIO region
                let offset = gpa - mmio_region.gpa.0;
                
                // Find and call the handler if one is registered
                if let Some(handler_name) = &mmio_region.handler {
                    // We need to get a mutable reference to the handler, but we only have a const pointer to partition
                    // This is safe because we're in a callback and the partition won't be modified concurrently
                    let partition_mut = &mut *(context as *mut crate::partition::Partition);
                    
                    if let Some(handler) = partition_mut.mmio_handlers_mut().get_mut(handler_name) {
                        match direction {
                            0 => {
                                // Read operation
                                match handler.handle_read(offset, access_size) {
                                    Ok(value) => {
                                        // Set the data based on access size
                                        // Data is [u8; 8], so we need to write bytes in little-endian order
                                        let value_bytes = value.to_le_bytes();
                                        match access_size {
                                            1 => {
                                                mem_info.Data[0] = value_bytes[0];
                                            }
                                            2 => {
                                                mem_info.Data[0..2].copy_from_slice(&value_bytes[0..2]);
                                            }
                                            4 => {
                                                mem_info.Data[0..4].copy_from_slice(&value_bytes[0..4]);
                                            }
                                            8 => {
                                                mem_info.Data.copy_from_slice(&value_bytes);
                                            }
                                            _ => {
                                                //eprintln!("[MMIO Emulator] Unsupported read size: {}", access_size);
                                                return windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0);
                                            }
                                        }
                                        windows::core::HRESULT(0) // S_OK
                                    }
                                    Err(e) => {
                                        //eprintln!("[MMIO Emulator] Handler read error: {:?}", e);
                                        windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INTERNAL_ERROR.0)
                                    }
                                }
                            }
                            1 => {
                                // Write operation
                                // Data is [u8; 8], read bytes in little-endian order
                                let value = match access_size {
                                    1 => mem_info.Data[0] as u64,
                                    2 => u16::from_le_bytes([mem_info.Data[0], mem_info.Data[1]]) as u64,
                                    4 => u32::from_le_bytes([
                                        mem_info.Data[0],
                                        mem_info.Data[1],
                                        mem_info.Data[2],
                                        mem_info.Data[3],
                                    ]) as u64,
                                    8 => u64::from_le_bytes([
                                        mem_info.Data[0],
                                        mem_info.Data[1],
                                        mem_info.Data[2],
                                        mem_info.Data[3],
                                        mem_info.Data[4],
                                        mem_info.Data[5],
                                        mem_info.Data[6],
                                        mem_info.Data[7],
                                    ]),
                                    _ => {
                                        //eprintln!("[MMIO Emulator] Unsupported write size: {}", access_size);
                                        return windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0);
                                    }
                                };
                                
                                match handler.handle_write(offset, access_size, value) {
                                    Ok(_) => windows::core::HRESULT(0), // S_OK
                                    Err(e) => {
                                        //eprintln!("[MMIO Emulator] Handler write error: {:?}", e);
                                        windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INTERNAL_ERROR.0)
                                    }
                                }
                            }
                            _ => {
                                //eprintln!("[MMIO Emulator] Unknown direction: {}", direction);
                                windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0)
                            }
                        }
                    } else {
                        //eprintln!("[MMIO Emulator] Handler '{}' not found for MMIO region {}", handler_name, mmio_region.name);
                        windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_NOT_FOUND.0)
                    }
                } else {
                    //eprintln!("[MMIO Emulator] No handler registered for MMIO region {}", mmio_region.name);
                    windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_NOT_FOUND.0)
                }
            } else {
                // Not an MMIO region - return error so emulator knows it can't handle this
                windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_NOT_FOUND.0)
            }
        }
    }

    unsafe extern "system" fn get_vp_registers_callback(
        context: *const std::ffi::c_void,
        register_names: *const WHV_REGISTER_NAME,
        register_count: u32,
        register_values: *mut WHV_REGISTER_VALUE,
    ) -> windows::core::HRESULT {
        unsafe {
            let partition = &*(context as *const crate::partition::Partition);
        
            // You need to call WHvGetVirtualProcessorRegisters here
            let result = WHvGetVirtualProcessorRegisters(
                partition.handle,
                0, // Assuming VP index 0 for now
                register_names,
                register_count,
                register_values,
            );

            if result.is_ok() { windows::core::HRESULT(0) } 
            else { windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INTERNAL_ERROR.0) }
        }
    }
    
    unsafe extern "system" fn set_vp_registers_callback(
        context: *const std::ffi::c_void,
        register_name: *const WHV_REGISTER_NAME,
        register_count: u32,
        register_value: *const WHV_REGISTER_VALUE,
    ) -> windows::core::HRESULT {
        unsafe {
            let partition = &*(context as *const crate::partition::Partition);
            let result = WHvSetVirtualProcessorRegisters(
                partition.handle,
                0, // Assuming VP index 0 for now
                register_name,
                register_count,
                register_value,
            );
            if result.is_ok() { windows::core::HRESULT(0) } 
            else { windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_INTERNAL_ERROR.0) }
        }
    }

    unsafe extern "system" fn translate_gva_page_callback(
        context: *const std::ffi::c_void,
        gva: u64,
        flags: WHV_TRANSLATE_GVA_FLAGS,
        result_code: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
        page_address: *mut u64,
    ) -> windows::core::HRESULT {
        unsafe {
            let partition = &*(context as *const crate::partition::Partition);
            let mut translation_result = WHV_TRANSLATE_GVA_RESULT::default();
            let mut gpa: u64 = 0;

            let hr = WHvTranslateGva(
                partition.handle,
                0, // VP index 0
                gva,
                flags,
                &mut translation_result,
                &mut gpa,
            );

            match hr {
                Ok(()) => {
                    *result_code = translation_result.ResultCode;
                    *page_address = gpa;
                    windows::core::HRESULT(0) // S_OK
                }
                Err(e) => {
                    e.code()
                }
            }
        }
    }
}
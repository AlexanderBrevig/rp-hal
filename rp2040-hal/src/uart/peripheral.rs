//! Universal Asynchronous Receiver Transmitter (UART)
//!
//! See [Chapter 4 Section 2](https://datasheets.raspberrypi.org/rp2040/rp2040_datasheet.pdf) of the datasheet for more details
//!
//! ## Usage
//!
//! See [examples/uart.rs](https://github.com/rp-rs/rp-hal/tree/main/rp2040-hal/examples/uart.rs) for a more complete example
//! ```no_run
//! use rp2040_hal::{clocks::init_clocks_and_plls, gpio::{Pins, FunctionUart}, pac, Sio, uart::{self, UartPeripheral}, watchdog::Watchdog};
//!
//! const XOSC_CRYSTAL_FREQ: u32 = 12_000_000; // Typically found in BSP crates
//!
//! let mut peripherals = pac::Peripherals::take().unwrap();
//! let sio = Sio::new(peripherals.SIO);
//! let pins = Pins::new(peripherals.IO_BANK0, peripherals.PADS_BANK0, sio.gpio_bank0, &mut peripherals.RESETS);
//! let mut watchdog = Watchdog::new(peripherals.WATCHDOG);
//! let mut clocks = init_clocks_and_plls(XOSC_CRYSTAL_FREQ, peripherals.XOSC, peripherals.CLOCKS, peripherals.PLL_SYS, peripherals.PLL_USB, &mut peripherals.RESETS, &mut watchdog).ok().unwrap();
//!
//! // Set up UART on GP0 and GP1 (Pico pins 1 and 2)
//! let pins = (
//!     pins.gpio0.into_mode::<FunctionUart>(),
//!     pins.gpio1.into_mode::<FunctionUart>(),
//! );
//! // Need to perform clock init before using UART or it will freeze.
//! let uart = UartPeripheral::new(peripherals.UART0, pins, &mut peripherals.RESETS)
//!     .enable(
//!         uart::common_configs::_9600_8_N_1,
//!         clocks.peripheral_clock.into(),
//!     )
//!     .unwrap();
//!
//! uart.write_full_blocking(b"Hello World!\r\n");
//! ```

use super::*;
use crate::pac::uart0::uartlcr_h::W as UART_LCR_H_Writer;
use core::convert::Infallible;
use core::fmt;
use embedded_hal::serial::{Read, Write};
use embedded_time::fixed_point::FixedPoint;
use embedded_time::rate::Baud;
use embedded_time::rate::Hertz;
use nb::Error::{Other, WouldBlock};
use rp2040_pac::{UART0, UART1};

#[cfg(feature = "eh1_0_alpha")]
use eh1_0_alpha::serial::nb as eh1;

/// An UART Peripheral based on an underlying UART device.
pub struct UartPeripheral<S: State, D: UartDevice, P: ValidUartPinout<D>> {
    device: D,
    _state: S,
    pins: P,
    config: UartConfig,
    effective_baudrate: Baud,
}

impl<S: State, D: UartDevice, P: ValidUartPinout<D>> UartPeripheral<S, D, P> {
    fn transition<To: State>(self, state: To) -> UartPeripheral<To, D, P> {
        UartPeripheral {
            device: self.device,
            pins: self.pins,
            config: self.config,
            effective_baudrate: self.effective_baudrate,
            _state: state,
        }
    }

    /// Releases the underlying device and pins.
    pub fn free(self) -> (D, P) {
        (self.device, self.pins)
    }
}

impl<D: UartDevice, P: ValidUartPinout<D>> UartPeripheral<Disabled, D, P> {
    /// Creates an UartPeripheral in Disabled state.
    pub fn new(device: D, pins: P, resets: &mut pac::RESETS) -> UartPeripheral<Disabled, D, P> {
        device.reset_bring_down(resets);
        device.reset_bring_up(resets);

        UartPeripheral {
            device,
            _state: Disabled,
            pins,
            config: common_configs::_9600_8_N_1, // placeholder
            effective_baudrate: Baud(0),
        }
    }

    /// Enables the provided UART device with the given configuration.
    pub fn enable(
        self,
        config: UartConfig,
        frequency: Hertz,
    ) -> Result<UartPeripheral<Enabled, D, P>, Error> {
        let (mut device, pins) = self.free();
        let effective_baudrate = configure_baudrate(&mut device, &config.baudrate, &frequency)?;

        device.uartlcr_h.write(|w| {
            w.fen().set_bit();
            set_format(w, &config.data_bits, &config.stop_bits, &config.parity);
            w
        });

        // Enable the UART, and the TX,RC,CTS and RTS based on the pins
        device.uartcr.write(|w| {
            w.uarten().set_bit();
            w.txe().bit(P::TX_ENABLED);
            w.rxe().bit(P::RX_ENABLED);
            w.ctsen().bit(P::CTS_ENABLED);
            w.rtsen().bit(P::RTS_ENABLED);

            w
        });

        device.uartdmacr.write(|w| {
            w.txdmae().set_bit();
            w.rxdmae().set_bit();
            w
        });

        Ok(UartPeripheral {
            device,
            config,
            pins,
            effective_baudrate,
            _state: Enabled,
        })
    }
}

impl<D: UartDevice, P: ValidUartPinout<D>> UartPeripheral<Enabled, D, P> {
    /// Disable this UART Peripheral, falling back to the Disabled state.
    pub fn disable(self) -> UartPeripheral<Disabled, D, P> {
        // Disable the UART, both TX and RX
        self.device.uartcr.write(|w| {
            w.uarten().clear_bit();
            w.txe().clear_bit();
            w.rxe().clear_bit();
            w.ctsen().clear_bit();
            w.rtsen().clear_bit();
            w
        });

        self.transition(Disabled)
    }

    /// Writes bytes to the UART.
    /// This function writes as long as it can. As soon that the FIFO is full, if :
    /// - 0 bytes were written, a WouldBlock Error is returned
    /// - some bytes were written, it is deemed to be a success
    /// Upon success, the remaining slice is returned.
    pub fn write_raw<'d>(&self, data: &'d [u8]) -> nb::Result<&'d [u8], Infallible> {
        super::writer::write_raw(&self.device, data)
    }

    /// Reads bytes from the UART.
    /// This function reads as long as it can. As soon that the FIFO is empty, if :
    /// - 0 bytes were read, a WouldBlock Error is returned
    /// - some bytes were read, it is deemed to be a success
    /// Upon success, it will return how many bytes were read.
    pub fn read_raw<'b>(&self, buffer: &'b mut [u8]) -> nb::Result<usize, ReadError<'b>> {
        super::reader::read_raw(&self.device, buffer)
    }

    /// Writes bytes to the UART.
    /// This function blocks until the full buffer has been sent.
    pub fn write_full_blocking(&self, data: &[u8]) {
        super::writer::write_full_blocking(&self.device, data);
    }

    /// Reads bytes from the UART.
    /// This function blocks until the full buffer has been received.
    pub fn read_full_blocking(&self, buffer: &mut [u8]) -> Result<(), ReadErrorType> {
        super::reader::read_full_blocking(&self.device, buffer)
    }

    /// Join the reader and writer halves together back into the original Uart peripheral.
    ///
    /// A reader/writer pair can be obtained by calling [`split`].
    ///
    /// [`split`]: #method.split
    pub fn join(reader: Reader<D, P>, writer: Writer<D, P>) -> Self {
        let _ = writer;
        Self {
            device: reader.device,
            _state: Enabled,
            pins: reader.pins,
            config: reader.config,
            effective_baudrate: reader.effective_baudrate,
        }
    }
}

impl<P: ValidUartPinout<UART0>> UartPeripheral<Enabled, UART0, P> {
    /// Split this peripheral into a separate reader and writer.
    pub fn split(self) -> (Reader<UART0, P>, Writer<UART0, P>) {
        let reader = Reader {
            device: self.device,
            pins: self.pins,
            config: self.config,
            effective_baudrate: self.effective_baudrate,
        };
        // Safety: reader and writer will never write to the same address
        let device_copy = unsafe { &*UART0::ptr() };
        let writer = Writer {
            device: device_copy,
            device_marker: core::marker::PhantomData,
            pins: core::marker::PhantomData,
        };
        (reader, writer)
    }
}

impl<P: ValidUartPinout<UART1>> UartPeripheral<Enabled, UART1, P> {
    /// Split this peripheral into a separate reader and writer.
    pub fn split(self) -> (Reader<UART1, P>, Writer<UART1, P>) {
        let reader = Reader {
            device: self.device,
            pins: self.pins,
            config: self.config,
            effective_baudrate: self.effective_baudrate,
        };
        // Safety: reader and writer will never write to the same address
        let device_copy = unsafe { &*UART1::ptr() };
        let writer = Writer {
            device: device_copy,
            device_marker: core::marker::PhantomData,
            pins: core::marker::PhantomData,
        };
        (reader, writer)
    }
}

/// The PL011 (PrimeCell UART) supports a fractional baud rate divider
/// From the wanted baudrate, we calculate the divider's two parts: integer and fractional parts.
/// Code inspired from the C SDK.
fn calculate_baudrate_dividers(
    wanted_baudrate: &Baud,
    frequency: &Hertz,
) -> Result<(u16, u16), Error> {
    // See Chapter 4, Section 2 §7.1 from the datasheet for an explanation of how baudrate is
    // calculated
    let baudrate_div = frequency
        .integer()
        .checked_mul(8)
        .and_then(|r| r.checked_div(wanted_baudrate.integer()))
        .ok_or(Error::BadArgument)?;

    Ok(match (baudrate_div >> 7, ((baudrate_div & 0x7F) + 1) / 2) {
        (0, _) => (1, 0),

        (int_part, _) if int_part >= 65535 => (65535, 0),

        (int_part, frac_part) => (int_part as u16, frac_part as u16),
    })
}

/// Baudrate configuration. Code loosely inspired from the C SDK.
fn configure_baudrate(
    device: &mut dyn UartDevice,
    wanted_baudrate: &Baud,
    frequency: &Hertz,
) -> Result<Baud, Error> {
    let (baud_div_int, baud_div_frac) = calculate_baudrate_dividers(wanted_baudrate, frequency)?;

    // First we load the integer part of the divider.
    device.uartibrd.write(|w| unsafe {
        w.baud_divint().bits(baud_div_int as u16);
        w
    });

    // Then we load the fractional part of the divider.
    device.uartfbrd.write(|w| unsafe {
        w.baud_divfrac().bits(baud_div_frac as u8);
        w
    });

    // PL011 needs a (dummy) line control register write to latch in the
    // divisors. We don't want to actually change LCR contents here.
    device.uartlcr_h.modify(|_, w| w);

    Ok(Baud(
        (4 * frequency.integer()) / (64 * baud_div_int + baud_div_frac) as u32,
    ))
}

/// Format configuration. Code loosely inspired from the C SDK.
fn set_format<'w>(
    w: &'w mut UART_LCR_H_Writer,
    data_bits: &DataBits,
    stop_bits: &StopBits,
    parity: &Option<Parity>,
) -> &'w mut UART_LCR_H_Writer {
    match parity {
        Some(p) => {
            w.pen().set_bit();
            match p {
                Parity::Odd => w.eps().clear_bit(),
                Parity::Even => w.eps().set_bit(),
            };
        }
        None => {
            w.pen().bit(false);
        }
    };

    unsafe {
        w.wlen().bits(match data_bits {
            DataBits::Five => 0b00,
            DataBits::Six => 0b01,
            DataBits::Seven => 0b10,
            DataBits::Eight => 0b11,
        })
    };

    match stop_bits {
        StopBits::One => w.stp2().clear_bit(),
        StopBits::Two => w.stp2().set_bit(),
    };

    w
}

impl<D: UartDevice, P: ValidUartPinout<D>> Read<u8> for UartPeripheral<Enabled, D, P> {
    type Error = ReadErrorType;

    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        let byte: &mut [u8] = &mut [0; 1];

        match self.read_raw(byte) {
            Ok(_) => Ok(byte[0]),
            Err(e) => match e {
                Other(inner) => Err(Other(inner.err_type)),
                WouldBlock => Err(WouldBlock),
            },
        }
    }
}

#[cfg(feature = "eh1_0_alpha")]
impl<D: UartDevice, P: ValidUartPinout<D>> eh1::Read<u8> for UartPeripheral<Enabled, D, P> {
    type Error = ReadErrorType;

    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        let byte: &mut [u8] = &mut [0; 1];

        match self.read_raw(byte) {
            Ok(_) => Ok(byte[0]),
            Err(e) => match e {
                Other(inner) => Err(Other(inner.err_type)),
                WouldBlock => Err(WouldBlock),
            },
        }
    }
}
impl<D: UartDevice, P: ValidUartPinout<D>> Write<u8> for UartPeripheral<Enabled, D, P> {
    type Error = Infallible;

    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        if self.write_raw(&[word]).is_err() {
            Err(WouldBlock)
        } else {
            Ok(())
        }
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        super::writer::transmit_flushed(&self.device)
    }
}

#[cfg(feature = "eh1_0_alpha")]
impl<D: UartDevice, P: ValidUartPinout<D>> eh1::Write<u8> for UartPeripheral<Enabled, D, P> {
    type Error = SerialInfallible;

    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        if self.write_raw(&[word]).is_err() {
            Err(WouldBlock)
        } else {
            Ok(())
        }
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        super::writer::transmit_flushed(&self.device).map_err(|e| match e {
            WouldBlock => WouldBlock,
            Other(v) => match v {},
        })
    }
}

impl<D: UartDevice, P: ValidUartPinout<D>> fmt::Write for UartPeripheral<Enabled, D, P> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        s.bytes()
            .try_for_each(|c| nb::block!(self.write(c)))
            .map_err(|_| fmt::Error)
    }
}

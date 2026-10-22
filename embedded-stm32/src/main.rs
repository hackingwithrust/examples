#![no_std]
#![no_main]

use bmpe280::BMP280;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};
use cortex_m::delay::Delay;
use cortex_m_rt::entry;
use defmt_rtt as _;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::i2c::I2c;
use embedded_hal::pwm::SetDutyCycle;
use stm32f4xx_hal::pac;
use stm32f4xx_hal::prelude::*;
use stm32f4xx_hal::rcc::Config;
use stm32f4xx_hal::serial::{Config as SerialConfig, Serial};

/// The BMP280's chip ID, read from register 0xD0 at start-up. A chip
/// that answers with anything else was not configured by the driver.
const BMP280_ID: u8 = 0x58;

/// The sensor's I2C address. The module this article wires up has its
/// SDO pin pulled low, which selects 0x76; with SDO left high the same
/// chip answers at 0x77 instead — and 0x77 is what the driver's own
/// `new` defaults to, which is why the address is passed explicitly.
const SENSOR_ADDRESS: u8 = 0x76;

/// The half-period the breath starts with, before the first reading
/// has arrived.
const DEFAULT_HALF: u16 = 300;

/// Ticks between sensor readings: half a second.
const READING_TICKS: u16 = 500;

/// The fastest half-period the mapping allows, in ticks. Below this a
/// breath stops being a breath and becomes a strobe.
const MIN_HALF: u16 = 50;

/// The slowest half-period the mapping allows, in ticks.
const MAX_HALF: u16 = 500;

/// Ticks shaved off the half-period per degree Celsius.
const TEMP_STEP: f64 = 10.0;

/// Loop ticks since start-up, one per millisecond. This is the
/// timestamp every defmt record carries: the firmware has no clock
/// peripheral wired up yet, and the loop's own pace is the clock
/// there is.
static TICKS: AtomicU32 = AtomicU32::new(0);

defmt::timestamp!("{=u32}", TICKS.load(Ordering::Relaxed));

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();
    let cp = cortex_m::peripheral::Peripherals::take().unwrap();

    // Constrain the RCC and apply the default clock configuration: the
    // chip's 16 MHz internal oscillator, no PLL.
    let mut rcc = dp.RCC.constrain().freeze(Config::default());
    let clocks = rcc.clocks;

    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpiob = dp.GPIOB.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);

    // PC13 is the board's user button, wired as step 5 left it.
    let button_pin = gpioc.pc13.into_pull_up_input();
    let mut button = Button::new(button_pin);

    // TIM2's channel 1 is muxed onto PA5, the pin LD2 is wired to. The
    // pin changes jobs here: no longer an output the firmware writes
    // outright, but a timer output the firmware only loads. One
    // kilohertz makes a period of 1 ms — the same length as the loop's
    // tick, so the duty is written once per period and never lands
    // mid-period.
    let (_, channels) = dp.TIM2.pwm_hz(1.kHz(), &mut rcc);
    let mut led = channels.0.with(gpioa.pa5.into_alternate::<1>());
    // The channel starts with its output disabled: the counter runs and
    // the waveform is generated, but nothing reaches the pin until this
    // call sets the channel's output-enable bit.
    led.enable();

    // I2C1's SCL and SDA are muxed onto PB8 and PB9 — the Nucleo's
    // Arduino header pins D15 and D14, where a sensor module plugs
    // straight in. The pins are passed in their plain split state: the
    // bus constructor moves them into alternate mode itself. One
    // hundred kilohertz is the bus's standard mode.
    let i2c = dp.I2C1.i2c((gpiob.pb8, gpiob.pb9), 100.kHz(), &mut rcc);
    let mut sensor = BMP280::new_with_address(i2c, SENSOR_ADDRESS);
    // The driver cannot fail, so it cannot report one either: a chip
    // that does not answer leaves it unconfigured, and every reading it
    // would return after that is a fiction. Ask the chip who it is
    // before trusting it.
    if sensor.id() != BMP280_ID {
        stall_lit();
    }

    // USART2's TX and RX are muxed onto PA2 and PA3, which the Nucleo
    // wires to the ST-Link's virtual COM port: the firmware's plain
    // text output shows up on the host as a serial device, with no
    // hardware beyond the board's own USB cable. The pins are handed
    // over plain, as the bus's were, and the constructor moves them
    // into alternate mode itself. `SerialConfig::default()` is 115200
    // baud, eight data bits, no parity, one stop bit.
    //
    // The `Result` is real — a baud rate the bus clock cannot produce
    // is an error — but 16 MHz divided down to 115200 is comfortably
    // inside the peripheral's range, so a failure here means the port
    // is unusable and the board says so the only way it can.
    let mut serial = match dp.USART2.serial(
        (gpioa.pa2, gpioa.pa3),
        SerialConfig::default(),
        &mut rcc,
    ) {
        Ok(serial) => serial,
        Err(_) => stall_lit(),
    };

    // The delay counts SysTick ticks against the AHB frequency.
    let mut delay = Delay::new(cp.SYST, clocks.hclk().to_Hz());

    sense(&mut sensor, &mut led, &mut button, &mut serial, &mut delay)
}

/// A button that reports one event per accepted press.
///
/// A mechanical contact does not close once: it bounces, and a raw read
/// of the pin flickers between levels around each transition. This
/// struct swallows that by only accepting a level that has held still
/// for `DEBOUNCE_TICKS` consecutive ticks. The pin type is generic, so
/// the same struct runs against a recorded trace on the host (that is
/// how it was tested) and against PC13 on the board.
struct Button<I: InputPin> {
    pin: I,
    /// The level the debouncer currently believes the line is at:
    /// `true` while the button is held down.
    pressed: bool,
    /// How many consecutive ticks have disagreed with `pressed`.
    held: u8,
}

/// A level change has to hold for this many consecutive 1 ms ticks before
/// it counts. Ten milliseconds is a starting point: long enough that
/// chatter cannot reach it, short enough that a deliberate press
/// registers on the first attempt.
const DEBOUNCE_TICKS: u8 = 10;

impl<I: InputPin> Button<I> {
    fn new(pin: I) -> Self {
        // A board starts with the button released; if it is held at
        // power-on, the first accepted level corrects this on release.
        Button {
            pin,
            pressed: false,
            held: 0,
        }
    }

    /// One tick of the poll loop. Returns `true` on the tick a press is
    /// accepted — not on every tick the button is held.
    fn tick(&mut self) -> bool {
        // Active low: the pin reads low while the button is pressed. An
        // error can't happen on this chip (`Infallible`), so a read that
        // did fail would keep the last accepted level.
        let last = self.pressed;
        let raw = self.pin.is_low().unwrap_or(last);
        if raw == self.pressed {
            self.held = 0;
            false
        } else {
            self.held += 1;
            if self.held >= DEBOUNCE_TICKS {
                self.pressed = raw;
                self.held = 0;
                // `raw` is `true` exactly when the new level is pressed;
                // a release is accepted the same way but not reported.
                raw
            } else {
                false
            }
        }
    }
}

/// The breathing curve, kept free of chip types so the same struct runs
/// against a mock PWM channel on a host (that is how it was tested) and
/// against TIM2's channel 1 on the board.
struct Breath {
    /// Ticks since the ramp started, 0..=2*half.
    phase: u16,
    /// Ticks in half a cycle: the ramp up, or the ramp down. The duty
    /// climbs from 0 to full and back inside 2*half ticks.
    half: u16,
    /// Whether the breath runs at all; the button flips this.
    running: bool,
}

impl Breath {
    fn new(half: u16) -> Self {
        Breath {
            phase: 0,
            half,
            running: true,
        }
    }

    /// A press switches the breathing off or on, and either way the next
    /// breath starts from dark.
    fn toggle(&mut self) {
        self.running = !self.running;
        self.phase = 0;
    }

    /// A new half-period restarts the ramp. A shorter half-period can
    /// leave `phase` past the new turnaround, where `duty` would count
    /// backwards from an underflow; restarting from dark costs at most
    /// one ramp.
    fn set_half(&mut self, half: u16) {
        self.half = half;
        if self.phase > half * 2 {
            self.phase = 0;
        }
    }

    /// The duty the LED should hold right now, in percent. The curve is
    /// a triangle: straight up to full brightness, straight back down,
    /// one tick wide at each turnaround.
    fn duty(&self) -> u8 {
        if !self.running {
            return 0;
        }
        // Integer arithmetic only — no float in sight, and none needed
        // for a straight line. The percent is the numerator over `half`.
        let percent = if self.phase < self.half {
            self.phase as u32 * 100
        } else {
            (2 * self.half as u32 - self.phase as u32) * 100
        } / self.half as u32;
        percent as u8
    }

    /// One tick of the loop. A paused breath does not advance.
    fn advance(&mut self) {
        if !self.running {
            return;
        }
        self.phase += 1;
        if self.phase > 2 * self.half {
            self.phase = 0;
        }
    }
}

/// The mapping from a reading to a breath: each degree Celsius shaves
/// `TEMP_STEP` ticks off the half-period, so a warmer room breathes
/// faster. The clamp is what keeps a silly reading — a dead bus reads
/// 0 °C, a hot day 45 — from stopping the breath or turning it into a
/// strobe.
fn half_for(temperature: f64) -> u16 {
    (MAX_HALF as f64 - TEMP_STEP * temperature).clamp(MIN_HALF as f64, MAX_HALF as f64) as u16
}

/// The loop, written against the traits instead of the chip's channel,
/// pin and bus types, as `breathe` was: `S` carries the bus the sensor
/// sits on, `P` the duty, `I` the button, `D` the time.
///
/// The serial port is the one concrete chip type in the list, because
/// this step's whole subject is what the firmware says about itself:
/// every reading is reported twice, once as a line of plain text on
/// the UART and once as a defmt record, and the two reports are the
/// comparison the article is about.
fn sense<S: I2c, P: SetDutyCycle, I: InputPin, D: DelayNs>(
    sensor: &mut BMP280<S>,
    led: &mut P,
    button: &mut Button<I>,
    serial: &mut Serial<stm32f4xx_hal::pac::USART2>,
    delay: &mut D,
) -> ! {
    let mut breath = Breath::new(DEFAULT_HALF);
    let mut since_reading: u16 = 0;

    loop {
        if button.tick() {
            breath.toggle();
            // A pause and a resume leave no trace on the board — the
            // LED going dark is all the signal there is — so they are
            // worth a record of their own.
            if breath.running {
                defmt::info!("breath resumed");
            } else {
                defmt::info!("breath paused");
            }
        }
        // One write per tick, running or paused. On this chip the error
        // type is `Infallible`, so the unwrap cannot fire.
        led.set_duty_cycle_percent(breath.duty()).unwrap();
        delay.delay_ms(1);
        TICKS.fetch_add(1, Ordering::Relaxed);
        breath.advance();

        since_reading += 1;
        if since_reading >= READING_TICKS {
            since_reading = 0;
            // The driver returns degrees Celsius as an `f64`; on this
            // chip that is a software double, and one reading every
            // half second is not where the cycles go.
            let reading = sensor.measure();
            breath.set_half(half_for(reading.temperature));
            // The same number, twice. The first report formats it into
            // text on the spot — the whole line, every character, goes
            // out over the wire. The second hands defmt the raw bytes
            // and a reference to the format string. `.ok()` because a
            // UART that cannot take a byte is not worth stopping the
            // firmware over.
            writeln!(serial, "temperature {:.2} C", reading.temperature).ok();
            defmt::info!("temperature {=f64} C", reading.temperature);
        }
    }
}

/// The board sits with LD2 lit: the look a panic gives, reused for a
/// sensor that does not answer and a serial port that cannot be
/// configured. Nothing else is running, so nothing else can take the
/// pin back — the mode bits are ours to rewrite.
fn stall_lit() -> ! {
    // Safety: `ptr()` returns the register block's address without
    // checking ownership. GPIOA exists at that address from reset
    // onward, and nothing else runs once this has been called.
    unsafe {
        let gpioa = pac::GPIOA::ptr();
        (*gpioa).moder().modify(|_, w| w.moder5().output());
        (*gpioa).bsrr().write(|w| w.bs5().set_());
    }
    loop {}
}

/// What runs when the firmware panics. Step 2 filled this slot with the
/// `panic-halt` crate; step 4 replaced that with a handler of its own.
///
/// Step 6 gave PA5 to the timer, and a pin in alternate mode is driven
/// by the peripheral: a BSRR write alone would no longer reach it. The
/// mode bits are the handler's to rewrite, so the pin goes back to a
/// plain output first and the LED is the BSRR's again — a panic still
/// freezes the board with LD2 lit, which tells it apart from a breath
/// that merely stopped.
///
/// Step 8 gives the handler a voice as well: the record goes out over
/// RTT before the LED lights. There is no `defmt::flush()` here on
/// purpose — flush blocks until the host has read the bytes, and with
/// the probe detached that would leave the board dark instead of lit.
/// The RTT buffer is in RAM and the probe reads it whenever it is
/// attached; what is lost is the record of a panic nobody was watching.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("panic: {=?}", info);
    stall_lit()
}

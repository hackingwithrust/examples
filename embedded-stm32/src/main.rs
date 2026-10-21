#![no_std]
#![no_main]

use cortex_m::delay::Delay;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use stm32f4xx_hal::pac;
use stm32f4xx_hal::prelude::*;
use stm32f4xx_hal::rcc::Config;

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();
    let cp = cortex_m::peripheral::Peripherals::take().unwrap();

    // Constrain the RCC and apply the default clock configuration: the
    // chip's 16 MHz internal oscillator, no PLL.
    let mut rcc = dp.RCC.constrain().freeze(Config::default());
    let clocks = rcc.clocks;

    // Splitting a port hands out its pins one at a time; the `&mut rcc`
    // is what turns the port's clock on. The button sits on port C, so
    // that port is split alongside the one the LED is on.
    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);
    let mut led = gpioa.pa5.into_push_pull_output();

    // PC13 is the board's user button. The line idles high and the press
    // pulls it low, so the pin reads low while the button is down. The
    // internal pull-up reinforces the idle level, which also keeps the
    // pin defined when nothing is wired to it.
    let button_pin = gpioc.pc13.into_pull_up_input();
    let mut button = Button::new(button_pin);

    // The delay counts SysTick ticks against the AHB frequency.
    let mut delay = Delay::new(cp.SYST, clocks.hclk().to_Hz());

    react(&mut led, &mut button, &mut delay)
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

/// The loop, written against the traits instead of the chip's pin and
/// timer types, as step 3's `blink` was: `P` drives the LED, `I` reads the
/// button, `D` counts the time.
///
/// Each accepted press toggles LD2, and the poll runs on a 1 ms cadence
/// so the debounce window is ten milliseconds of wall time.
fn react<P: OutputPin, I: InputPin, D: DelayNs>(
    led: &mut P,
    button: &mut Button<I>,
    delay: &mut D,
) -> ! {
    let mut lit = false;
    loop {
        if button.tick() {
            lit = !lit;
            if lit {
                led.set_high().unwrap();
            } else {
                led.set_low().unwrap();
            }
        }
        delay.delay_ms(1);
    }
}

/// What runs when the firmware panics. Step 2 filled this slot with the
/// `panic-halt` crate; this step writes the handler out instead.
///
/// `react` owns the LED pin and a panic ends that loop for good, so there
/// is no pin to reach for here. GPIOA sits at a fixed address for as long
/// as the chip has power, and the pin is still configured as a push-pull
/// output — mode bits survive. Setting bit 5 of the port's bit set/reset
/// register lights LD2 and leaves it lit: a panic now freezes the board
/// with the LED on instead of mid-blink, which tells the two apart.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Safety: `ptr()` returns the register block's address without
    // checking ownership. GPIOA exists at that address from reset onward,
    // nothing else runs once a panic has fired, and BSRR is write-only:
    // a set-bit write cannot race another reader or writer of the pin.
    unsafe {
        (*pac::GPIOA::ptr()).bsrr().write(|w| w.bs5().set_());
    }
    loop {}
}

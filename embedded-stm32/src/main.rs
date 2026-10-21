#![no_std]
#![no_main]

use cortex_m::delay::Delay;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::pwm::SetDutyCycle;
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

    let gpioa = dp.GPIOA.split(&mut rcc);
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

    // The delay counts SysTick ticks against the AHB frequency.
    let mut delay = Delay::new(cp.SYST, clocks.hclk().to_Hz());

    breathe(&mut led, &mut button, &mut delay)
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

/// The loop, written against the traits instead of the chip's channel
/// and pin types, as `react` was: `P` carries the duty, `I` reads the
/// button, `D` counts the time.
///
/// The LED is no longer switched, it is dimmed: one duty write per
/// millisecond, and each accepted press pauses or resumes the breath.
fn breathe<P: SetDutyCycle, I: InputPin, D: DelayNs>(
    led: &mut P,
    button: &mut Button<I>,
    delay: &mut D,
) -> ! {
    // Half a second up, half a second down: one breath takes a second.
    let mut breath = Breath::new(500);

    loop {
        if button.tick() {
            breath.toggle();
        }
        // One write per tick, running or paused. On this chip the error
        // type is `Infallible`, so the unwrap cannot fire.
        led.set_duty_cycle_percent(breath.duty()).unwrap();
        delay.delay_ms(1);
        breath.advance();
    }
}

/// What runs when the firmware panics. Step 2 filled this slot with the
/// `panic-halt` crate; step 4 replaced that with the handler below.
///
/// Step 6 gave PA5 to the timer, and a pin in alternate mode is driven
/// by the peripheral: a BSRR write alone would no longer reach it. The
/// mode bits are the handler's to rewrite, so the pin goes back to a
/// plain output first and the LED is the BSRR's again — a panic still
/// freezes the board with LD2 lit, which tells it apart from a breath
/// that merely stopped.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Safety: `ptr()` returns the register block's address without
    // checking ownership. GPIOA exists at that address from reset
    // onward, and nothing else runs once a panic has fired.
    unsafe {
        let gpioa = pac::GPIOA::ptr();
        (*gpioa).moder().modify(|_, w| w.moder5().output());
        (*gpioa).bsrr().write(|w| w.bs5().set_());
    }
    loop {}
}

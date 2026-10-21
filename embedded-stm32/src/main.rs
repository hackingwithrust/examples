#![no_std]
#![no_main]

use panic_halt as _;

use cortex_m::delay::Delay;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
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
    // is what turns the port's clock on.
    let gpioa = dp.GPIOA.split(&mut rcc);
    let mut led = gpioa.pa5.into_push_pull_output();

    // The delay counts SysTick ticks against the AHB frequency.
    let mut delay = Delay::new(cp.SYST, clocks.hclk().to_Hz());

    blink(&mut led, &mut delay)
}

/// The loop, written against the two traits instead of the chip's pin and
/// timer types: `P` is whatever drives the LED, `D` is whatever counts the
/// time.
fn blink<P: OutputPin, D: DelayNs>(led: &mut P, delay: &mut D) -> ! {
    loop {
        led.set_high().unwrap();
        delay.delay_ms(500);
        led.set_low().unwrap();
        delay.delay_ms(500);
    }
}

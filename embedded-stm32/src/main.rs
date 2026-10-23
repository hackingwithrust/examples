#![no_std]
#![no_main]

use bmpe280::BMP280;
use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use cortex_m::peripheral::{NVIC, SCB};
use critical_section::{CriticalSection, Mutex};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Timer};
use embassy_time_driver::{Driver, time_driver_impl};
use embassy_time_queue_utils::Queue;
use embedded_hal::digital::InputPin;
use embedded_hal::pwm::SetDutyCycle;
use stm32f4xx_hal::gpio::{Edge, Pin};
use stm32f4xx_hal::i2c::I2c;
use stm32f4xx_hal::pac;
use stm32f4xx_hal::pac::interrupt;
use stm32f4xx_hal::prelude::*;
use stm32f4xx_hal::rcc::Config;
use stm32f4xx_hal::serial::{Config as SerialConfig, Tx};
use stm32f4xx_hal::timer::PwmChannel;

/// The BMP280's chip ID, read from register 0xD0 at start-up. A chip
/// that answers with anything else was not configured by the driver.
const BMP280_ID: u8 = 0x58;

/// The sensor's I2C address. The module this article wires up has its
/// SDO pin pulled low, which selects 0x76; with SDO left high the same
/// chip answers at 0x77 instead — and 0x77 is what the driver's own
/// `new` defaults to, which is why the address is passed explicitly.
const SENSOR_ADDRESS: u8 = 0x76;

/// The half-period the breath starts with, in ticks. A tick is ten
/// milliseconds in the executor, so this is a third of a second —
/// the same half-period step 8's loop ran at, on a finer clock.
const DEFAULT_HALF: u16 = 30;

/// The fastest half-period the mapping allows, in ticks. Below this a
/// breath stops being a breath and becomes a strobe.
const MIN_HALF: u16 = 5;

/// The slowest half-period the mapping allows, in ticks.
const MAX_HALF: u16 = 50;

/// Ticks shaved off the half-period per degree Celsius. One tick is
/// ten milliseconds, so a degree is worth a tenth of a second.
const TEMP_STEP: f64 = 1.0;

/// A level change has to hold for this many consecutive 1 ms ticks
/// before it counts. Ten milliseconds is a starting point: long enough
/// that chatter cannot reach it, short enough that a deliberate press
/// registers on the first attempt.
const DEBOUNCE_TICKS: u8 = 10;

/// Ticks the button has to be held before the chip is put to sleep. The
/// button task polls once a millisecond, so this is two seconds: long
/// enough that nobody stops the chip by accident, short enough that the
/// wait does not feel broken.
const HOLD_TICKS: u32 = 2_000;

// Every record's timestamp is the same millisecond count the tasks
// wait on, read straight from the time driver. Step 8 kept this
// counter by hand in the loop; the loop is gone, and the clock it was
// standing in for is now real hardware.
defmt::timestamp!("{=u64}", embassy_time_driver::now());

/// One thing the firmware has to say about itself. Step 8 wrote each
/// of these on the spot, from inside the loop that produced them; they
/// now travel to the task that owns the ports, and that task is the
/// only place either port is touched.
enum Report {
    /// A temperature reading, in degrees Celsius.
    Temperature(f64),
    /// Whether the breath is running: `true` resumed, `false` paused.
    Breath(bool),
    /// The chip came back from Stop mode.
    Awake,
}

/// Reports waiting for the serial port. The logger is the only reader
/// and both producers are slower than it is, so eight slots never fill.
static REPORTS: Channel<CriticalSectionRawMutex, Report, 8> = Channel::new();

/// The latest temperature reading. A `Watch` rather than a channel
/// because the breath wants the current value whenever it looks, not
/// every value that ever arrived — one slot, overwritten in place.
static TEMPERATURE: Watch<CriticalSectionRawMutex, f64, 1> = Watch::new();

/// A press that has been accepted and not yet acted on. The button
/// task raises it; the breath task waits on it.
static PRESS: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// A hold long enough to stop the chip. The button task raises it; the
/// power task waits on it.
static SLEEP: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// TIM5's registers, reached through the pointer the PAC hands out.
/// The driver configures the timer once and only the interrupt handler
/// touches it after that, so a second handle to the same block is the
/// normal way to reach it — the owned `TIM5` singleton is never moved
/// anywhere.
fn tim5() -> &'static pac::tim5::RegisterBlock {
    // Safety: TIM5 exists at this address from reset onward, and its
    // clock is enabled before the interrupt that reads it is unmasked.
    unsafe { &*pac::TIM5::ptr() }
}

/// The firmware's clock, and the reason any task can wait at all.
///
/// embassy-time has no clock of its own. It calls out to a driver
/// through two functions — `now` and `schedule_wake` — and whoever
/// provides them decides what a tick is and how long it lasts. This is
/// that provider: TIM5 counting milliseconds, stretched to 64 bits in
/// software, with the queue of pending wakeups shared between the
/// tasks and the timer interrupt.
struct Tim5Driver {
    /// Cleared until the timer registers have been configured. The
    /// `Driver` trait requires `now` to answer even before the
    /// hardware is ready, and zero is the only honest answer there is.
    started: AtomicBool,
    /// Counter wraps since boot. Only the update interrupt writes it,
    /// so a plain load is a correct read.
    wraps: AtomicU32,
    /// Wakeups embassy-time has asked for, shared with the timer
    /// interrupt: a task adds one while it is being polled, the
    /// interrupt takes one out when it is due.
    queue: Mutex<RefCell<Queue>>,
}

time_driver_impl!(static DRIVER: Tim5Driver = Tim5Driver {
    started: AtomicBool::new(false),
    wraps: AtomicU32::new(0),
    queue: Mutex::new(RefCell::new(Queue::new())),
});

impl Tim5Driver {
    /// Configure TIM5 as a 1 kHz tick source and start it. Called once,
    /// before the first task runs.
    fn init(&self) {
        let tim = tim5();

        // APB1 carries the 16 MHz internal oscillator under the default
        // clock configuration, so the prescaler divides by 16 000 for
        // one tick per millisecond. The prescaler and the reload value
        // only take effect on an update event, and an update event with
        // the update interrupt live would fire it at once — so the
        // counter is stopped, the registers are loaded, and one update
        // is generated by hand with the update interrupt still muted.
        tim.cr1().modify(|_, w| w.cen().disabled());
        tim.psc().write(|w| w.psc().set(15_999));
        // A 32-bit counter at 1 kHz wraps after 2^32 ticks, a little
        // under 50 days. The wrap is counted here in software rather
        // than being the timebase itself.
        tim.arr().write(|w| w.arr().set(0xFFFF_FFFF));
        // URS counter-only: the manual update below reloads the
        // registers without setting the update flag.
        tim.cr1().modify(|_, w| w.urs().counter_only());
        tim.egr().write(|w| w.ug().update());
        tim.cr1().modify(|_, w| w.urs().any_event());
        // The wrap counter needs the update interrupt. The compare
        // interrupt stays off until something is actually armed.
        tim.dier().modify(|_, w| w.uie().enabled());
        tim.cr1().modify(|_, w| w.cen().enabled());

        self.started.store(true, Ordering::Relaxed);
    }

    /// One more counter wrap, from the update interrupt.
    fn on_wrap(&self) {
        self.wraps.fetch_add(1, Ordering::Release);
    }

    /// The compare interrupt fired: wake everything that is due and arm
    /// the next alarm.
    fn on_alarm(&self, cs: CriticalSection) {
        let mut queue = self.queue.borrow(cs).borrow_mut();
        let mut next = queue.next_expiration(self.now());
        while !self.set_alarm(next) {
            next = queue.next_expiration(self.now());
        }
    }

    /// Arm the compare register for `timestamp`, or report that it
    /// cannot be armed. `false` means the caller must look at the queue
    /// again: the deadline has already passed.
    ///
    /// Called with the queue's lock held, from `schedule_wake` and from
    /// the timer interrupt — never from two places at once, which is
    /// what makes the read-modify-write on DIER safe.
    fn set_alarm(&self, timestamp: u64) -> bool {
        let tim = tim5();

        if timestamp == u64::MAX {
            // Nothing is scheduled: switch the compare interrupt off
            // and leave it off.
            tim.dier().modify(|_, w| w.cc1ie().disabled());
            return true;
        }

        let now = self.now();
        if timestamp <= now {
            // Due already. Disarm and let the caller take the next
            // deadline from the queue, which will wake this one.
            tim.dier().modify(|_, w| w.cc1ie().disabled());
            return false;
        }

        // How far away the deadline is. The compare register is 32 bits
        // wide, like the counter, so anything closer than a full wrap is
        // written directly. A deadline further out than that is armed
        // for the end of the current period instead: the compare fires,
        // finds nothing due, and arms the next period — one interrupt
        // per 49 days is a fair price for a compare register that is
        // not 64 bits wide.
        let diff = timestamp - now;
        let ccr = if diff >= 0xFFFF_FFFF {
            0xFFFF_FFFF
        } else {
            timestamp as u32
        };
        tim.ccr1().write(|w| w.ccr().set(ccr));
        tim.dier().modify(|_, w| w.cc1ie().enabled());

        // The counter may have reached the compare value while it was
        // being written. If the deadline has passed since, say so and
        // let the caller retry.
        self.now() < timestamp
    }
}

impl Driver for Tim5Driver {
    /// Ticks since boot: the software wrap count in the high word, the
    /// counter in the low one.
    fn now(&self) -> u64 {
        if !self.started.load(Ordering::Relaxed) {
            return 0;
        }
        let tim = tim5();
        loop {
            // The counter is read twice around the wrap count. If it
            // went backwards between the two reads, a wrap raced the
            // first one and the wrap count just read is stale: start
            // over.
            let first = tim.cnt().read().cnt().bits() as u64;
            let wraps = self.wraps.load(Ordering::Acquire);
            let second = tim.cnt().read().cnt().bits() as u64;
            if first <= second {
                return (wraps as u64) << 32 | second;
            }
        }
    }

    fn schedule_wake(&self, at: u64, waker: &core::task::Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            if queue.schedule_wake(at, waker) {
                let mut next = queue.next_expiration(self.now());
                while !self.set_alarm(next) {
                    next = queue.next_expiration(self.now());
                }
            }
        })
    }
}

/// The timer interrupt: one vector for both of TIM5's events, the
/// counter wrap and the alarm.
#[cortex_m_rt::interrupt]
fn TIM5() {
    let tim = tim5();
    let sr = tim.sr().read();
    // The status flags clear by writing a zero, so the register is
    // written back inverted: every flag that was set is cleared and the
    // ones that were not stay clear. Reading first and writing the
    // inverse cannot miss a flag that arrived in between.
    //
    // Safety: writing a zero to these flags is their documented clear
    // condition, and the inverted write touches no other field. The
    // block covers both unsafe calls: the write itself and the
    // raw-bits writer inside the closure.
    unsafe {
        tim.sr().write_with_zero(|w| w.bits(!sr.bits()));
    }

    if sr.uif().bit() {
        DRIVER.on_wrap();
    }
    if sr.cc1if().bit() {
        critical_section::with(|cs| DRIVER.on_alarm(cs));
    }
}

/// The button's interrupt: one vector shared by EXTI lines 10 to 15, of
/// which this firmware uses one. The handler's whole job is to clear
/// the pending bit. While the chip is awake the poll loop in
/// `watch_button` is what acts on the press — this interrupt only stops
/// itself from re-entering. While the chip is in Stop mode this
/// interrupt is the wake-up: the line brings the core back, the handler
/// runs, and the firmware continues where `enter_stop_mode` left off.
#[cortex_m_rt::interrupt]
fn EXTI15_10() {
    clear_button_interrupt();
}

/// Clear the pending bit on EXTI line 13 — PC13's line — without
/// owning the pin the line is wired to. The button task owns the pin,
/// so the interrupt handler and the stop-mode entry both reach the
/// line through the block's address instead.
fn clear_button_interrupt() {
    // Safety: `ptr()` hands out the EXTI block's address without
    // checking ownership. The block exists at that address from reset
    // onward. Writing a one to a pending bit is its documented clear
    // condition, so this write touches line 13 and leaves the others
    // exactly as they were.
    unsafe {
        (*pac::EXTI::ptr()).pr().write(|w| w.bits(1 << 13));
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let dp = pac::Peripherals::take().unwrap();

    // Constrain the RCC and apply the default clock configuration: the
    // chip's 16 MHz internal oscillator, no PLL. APB1 runs at the same
    // 16 MHz, which is the clock the time driver divides.
    let mut rcc = dp.RCC.constrain().freeze(Config::default());

    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpiob = dp.GPIOB.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);

    // TIM5 hangs off APB1 and nothing in the HAL takes it over, so its
    // clock is enabled here by hand: without it the counter never runs
    // and every `Timer` in the firmware waits forever.
    rcc.apb1enr().modify(|_, w| w.tim5en().enabled());

    // The PWR block is the same story: the HAL has no stop-mode API, so
    // nothing else enables its clock, and a register write with the
    // block's clock off does not land.
    rcc.apb1enr().modify(|_, w| w.pwren().enabled());

    // The clock every task waits on comes up before anything can wait
    // on it: the driver configures TIM5 and unmasks its interrupt here,
    // and the first task is not spawned until that is done.
    DRIVER.init();
    NVIC::unpend(pac::Interrupt::TIM5);
    unsafe {
        NVIC::unmask(pac::Interrupt::TIM5);
    }

    // The two blocks the EXTI setup writes: SYSCFG picks which pin
    // feeds line 13, and EXTI holds the edge and mask bits themselves.
    let mut syscfg = dp.SYSCFG.constrain(&mut rcc);
    let mut exti = dp.EXTI;

    // PC13 is the board's user button, wired as step 5 left it. The
    // EXTI line behind the pin is armed here, once, and stays armed:
    // it is what wakes the chip from Stop mode, and while the chip is
    // awake it costs an interrupt whose only job is to clear a pending
    // bit. The pin itself stays in the button task's hands, polled as
    // before — the line and the poll see the same press, and only the
    // poll acts on it.
    let mut pc13 = gpioc.pc13.into_pull_up_input();
    // The line has to be told which pin feeds it: SYSCFG's fourth
    // external interrupt configuration register maps EXTI13 to port C.
    pc13.make_interrupt_source(&mut syscfg);
    // Both edges, so a release wakes the chip too. A press that is
    // still held when the chip stops is no longer an edge — the line
    // has already fallen — so without the rising edge the user would
    // have to keep holding the button to keep the chip asleep.
    pc13.trigger_on_edge(&mut exti, Edge::RisingFalling);
    pc13.enable_interrupt(&mut exti);
    let button = Button::new(pc13);
    unsafe {
        NVIC::unmask(pac::Interrupt::EXTI15_10);
    }

    // TIM2's channel 1 is muxed onto PA5, the pin LD2 is wired to. The
    // pin changes jobs here: no longer an output the firmware writes
    // outright, but a timer output the firmware only loads. One
    // kilohertz makes a period of 1 ms.
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
    let serial = match dp.USART2.serial(
        (gpioa.pa2, gpioa.pa3),
        SerialConfig::default(),
        &mut rcc,
    ) {
        Ok(serial) => serial,
        Err(_) => stall_lit(),
    };
    // The port splits in two: the transmitter moves into the task that
    // owns the logging, and the receiver is dropped — nothing reads it,
    // as in step 8.
    let (tx, _rx) = serial.split();

    // One job per task, and each task owns the hardware it needs. What
    // the tasks have to say to each other travels over the statics
    // above, not through shared borrows that would have to outlive the
    // executor's poll of them.
    spawner.spawn(breathe(led, &TEMPERATURE, &REPORTS).unwrap());
    spawner.spawn(watch_button(button).unwrap());
    spawner.spawn(read_sensor(sensor, &TEMPERATURE, &REPORTS).unwrap());
    spawner.spawn(report(tx, &REPORTS).unwrap());
    // The power task owns the two blocks the stop-mode entry touches:
    // the PWR registers, and the core's System Control Block, which is
    // where the SLEEPDEEP bit lives. Taking them here rather than
    // inside the task keeps every `take()` in the firmware in one
    // place, and the executor's own idle — a `wfe` that sleeps shallowly
    // — never sees the bit set.
    let cp = cortex_m::peripheral::Peripherals::take().unwrap();
    spawner.spawn(power(dp.PWR, cp.SCB, &REPORTS).unwrap());
}

/// The breath: one step of the ramp every ten milliseconds, and a
/// pause or resume when a press arrives.
///
/// The task owns the PWM channel. It shares the temperature `Watch`
/// and the report `Channel`, which is why both are borrowed for
/// `'static`: a task argument has to live as long as the executor, and
/// a static does.
#[embassy_executor::task]
async fn breathe(
    mut led: PwmChannel<pac::TIM2, 0>,
    temperature: &'static Watch<CriticalSectionRawMutex, f64, 1>,
    reports: &'static Channel<CriticalSectionRawMutex, Report, 8>,
) {
    let mut breath = Breath::new(DEFAULT_HALF);

    loop {
        // Two things can happen in the next ten milliseconds: the tick
        // comes due, or a press arrives. Whichever comes first is what
        // this task does; the other waits. `select` polls both futures
        // and drops the one that lost.
        match select(PRESS.wait(), Timer::after(Duration::from_millis(10))).await {
            Either::First(()) => {
                breath.toggle();
                // A pause and a resume leave no trace on the board —
                // the LED going dark is all the signal there is — so
                // they are worth a report of their own.
                reports.send(Report::Breath(breath.running)).await;
            }
            Either::Second(()) => {
                // A new reading only changes the half-period; there is
                // nothing to do on the ticks in between.
                if let Some(t) = temperature.try_get() {
                    breath.set_half(half_for(t));
                }
                breath.advance();
                // One write per tick, running or paused. On this chip
                // the error type is `Infallible`, so the unwrap cannot
                // fire.
                led.set_duty_cycle_percent(breath.duty()).unwrap();
            }
        }
    }
}

/// The button: one poll per millisecond, the same debounce step 5
/// settled on, and one signal per accepted press. The hold counter is
/// new: a press held for `HOLD_TICKS` raises `SLEEP`, and the power
/// task is what acts on it.
#[embassy_executor::task]
async fn watch_button(mut button: Button<Pin<'C', 13>>) {
    // Ticks the button has been held. Saturating rather than wrapping:
    // a button held for an hour is still just a button held, and the
    // count is not the signal — `HOLD_TICKS` is.
    let mut held: u32 = 0;
    loop {
        if button.tick() {
            // The debouncer reports one event per accepted press. The
            // signal carries it to the breath task; a press that
            // arrives while the breath task is mid-tick waits there
            // until it looks.
            PRESS.signal(());
        }
        if button.pressed() {
            held = held.saturating_add(1);
            if held == HOLD_TICKS {
                // A hold is not a press: the breath has already
                // paused, and this says the chip should stop.
                SLEEP.signal(());
            }
        } else {
            held = 0;
        }
        Timer::after(Duration::from_millis(1)).await;
    }
}

/// The sensor: one reading every half second, handed to whoever wants
/// it.
#[embassy_executor::task]
async fn read_sensor(
    mut sensor: BMP280<I2c<pac::I2C1>>,
    temperature: &'static Watch<CriticalSectionRawMutex, f64, 1>,
    reports: &'static Channel<CriticalSectionRawMutex, Report, 8>,
) {
    loop {
        Timer::after(Duration::from_millis(500)).await;
        // The driver returns degrees Celsius as an `f64`; on this chip
        // that is a software double, and one reading every half second
        // is not where the cycles go. The measurement itself is a
        // blocking I2C transfer: seven bytes at 100 kHz is a little
        // over half a millisecond, so the task holds the executor for
        // that long and then gives it back. Nothing else is due in the
        // window, and nothing is late because of it.
        let reading = sensor.measure();
        // The breath paces itself on the latest value whenever it
        // looks; the logger gets every one.
        temperature.sender().send(reading.temperature);
        reports.send(Report::Temperature(reading.temperature)).await;
    }
}

/// The logger: the only task that touches either port.
#[embassy_executor::task]
async fn report(mut serial: Tx<pac::USART2>, reports: &'static Channel<CriticalSectionRawMutex, Report, 8>) {
    loop {
        // Waiting here until something is said is the point: the port
        // is free for the rest of the time, and nothing else has to
        // take a lock to use it.
        match reports.receive().await {
            Report::Temperature(t) => {
                // The same number, twice: once as a line of text on the
                // UART, once as a defmt record over RTT. `.ok()` because
                // a UART that cannot take a byte is not worth stopping
                // the firmware over.
                writeln!(serial, "temperature {:.2} C", t).ok();
                defmt::info!("temperature {=f64} C", t);
            }
            Report::Breath(true) => {
                writeln!(serial, "breath resumed").ok();
                defmt::info!("breath resumed");
            }
            Report::Breath(false) => {
                writeln!(serial, "breath paused").ok();
                defmt::info!("breath paused");
            }
            Report::Awake => {
                // No duration: the executor's clock stopped with the
                // chip, so there is nothing honest to measure the
                // sleep against. The record is the event, not the
                // length.
                writeln!(serial, "awake").ok();
                defmt::info!("awake");
            }
        }
    }
}

/// The chip's power state: the one task that stops the chip, and the
/// only one that knows what woke it.
///
/// Everything else in the firmware runs whether or not anyone is
/// watching. This task waits for a hold, stops the chip, and reports
/// the wake-up — the shape a battery-powered build would give the
/// whole firmware, with a clock in place of the finger.
#[embassy_executor::task]
async fn power(
    mut pwr: pac::PWR,
    mut scb: SCB,
    reports: &'static Channel<CriticalSectionRawMutex, Report, 8>,
) {
    loop {
        SLEEP.wait().await;
        enter_stop_mode(&mut pwr, &mut scb);
        reports.send(Report::Awake).await;
    }
}

/// Put the chip in Stop mode, and return when the EXTI line on PC13
/// wakes it.
///
/// Stop mode stops every clock in the 1.2 V domain — the CPU, every
/// peripheral, and the HSI oscillator the whole firmware runs on —
/// while the SRAM and the register contents survive (RM0368 § 5.3.4).
/// What brings the chip back is the line armed on PC13 in `main`: any
/// enabled EXTI line wakes the chip from Stop mode, and that line is
/// enabled.
///
/// The clock the executor keeps time with stops with the chip: TIM5 is
/// on APB1, and APB1 is inside the stopped domain. Every `Timer` that
/// was pending when the chip stopped comes due late, by however long
/// the chip was stopped — the counter resumes where it froze instead
/// of catching up, so nothing is lost and nothing is on time. A
/// firmware that needs to know the time across a stop has to move its
/// time driver to the RTC, which keeps counting on its own crystal;
/// that is a bigger change than this step makes, and it is why the
/// wake-up report carries no duration.
fn enter_stop_mode(pwr: &mut pac::PWR, scb: &mut SCB) {
    // One write picks the flavour of Stop: PDDS clear for Stop rather
    // than Standby, LPDS for the low-power regulator, FPDS to power the
    // flash down with it. That combination is STOP LPFPD in RM0368's
    // Table 18 — the slowest to wake and the cheapest to run, 12 µA
    // typical at 3.3 V and 25 °C (DS10086, Table 28) against 42 µA
    // with the flash left running. The rest of the register — VOS, the
    // PVD, the backup domain — is read-modify-written back unchanged.
    pwr.cr()
        .modify(|_, w| w.pdds().enter_stop().lpds().low_power().fpds().power_down());

    // A bounce that arrived while the hold was being recognised has
    // left a pending bit behind, and WFI with a pending interrupt
    // returns at once instead of sleeping. Clearing the line's bit and
    // the NVIC's makes the stop the default; an edge that lands after
    // this still wins, which is the right outcome — something
    // happened.
    NVIC::unpend(pac::Interrupt::EXTI15_10);
    clear_button_interrupt();

    // SLEEPDEEP is what makes the next WFI a deep sleep. The
    // executor's idle is a `wfe`, which sleeps shallowly; setting the
    // bit here and clearing it on the way out is what keeps the two
    // apart, so the executor can never stop the chip by accident.
    scb.set_sleepdeep();
    cortex_m::asm::wfi();
    scb.clear_sleepdeep();

    // The wake-up flag says the chip came back from a low-power mode.
    // Clearing it here leaves the next Stop starting from a clean
    // slate, so a stale flag can never be mistaken for a fresh wake.
    pwr.cr().modify(|_, w| w.cwuf().clear());
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

    /// Whether the debouncer believes the button is down right now.
    /// The hold counter needs this: `tick` reports edges, and a hold is
    /// not an edge — it is a level that lasts.
    fn pressed(&self) -> bool {
        self.pressed
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
///
/// A panic in a task lands here too: the executor polls tasks in thread
/// mode, so a panic in a task is a panic on the spot, and the board
/// stops with the LED lit exactly as it did when the loop panicked.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("panic: {=?}", info);
    stall_lit()
}

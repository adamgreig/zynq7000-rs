//! Zedboard ethernet example code.
//!
//! This code uses embassy-net, a smoltcp based networking stack, as the IP stack.
//! It uses DHCP by default to assign the IP address. The assigned address will be displayed on
//! the console.
//!
//! Alternatively, you can also set a static IPv4 configuration via the `STATIC_IPV4_CONFIG`
//! constant and by setting `USE_DHCP` to `false`.
//!
//! It also exposes simple UDP and TCP echo servers. You can use the following sample commands
//! to send UDP or TCP data to the Zedboard using the Unix `netcat` application:
//!
//! ## UDP
//!
//! ```sh
//! echo "Hello Zedboard" | nc -uN <ip-address> 8000
//! ```
//!
//! ## TCP
//!
//! ```sh
//! echo "Hello Zedboard" | nc -N <ip-address> 8000
//! ```
#![no_std]
#![no_main]

use aarch32_cpu::asm::nop;
use core::{net::Ipv4Addr, panic::PanicInfo};
use embassy_executor::Spawner;
use embassy_net::{Ipv4Cidr, StaticConfigV4, tcp::TcpSocket, udp::UdpSocket};
use embassy_time::{Duration, Timer};
use embedded_io::Write;
use embedded_io_async::Write as _;
use rand::{Rng, SeedableRng};
use zedboard::PS_CLOCK_FREQUENCY;
use zynq7000_hal::{
    BootMode,
    clocks::Clocks,
    configure_level_shifter,
    eth::{
        AlignedBuffer, ClockDivSet, EthernetConfig, EthernetLowLevel, embassy_net::InterruptResult,
        Speed, Duplex,
    },
    generic_interrupt_handler,
    gic::{Configurator, Interrupt},
    gpio::{GpioPins, Output, PinState},
    gtc::GlobalTimerCounter,
    l2_cache,
};

use zynq7000::{Peripherals, slcr::LevelShifterConfig};
use zynq7000_rt::{self as _, mmu::section_attrs::SHAREABLE_DEVICE, mmu_l1_table_mut};
use defmt_rtt as _;

#[used]
#[unsafe(no_mangle)]
static FIRMWARE_COMMIT: &'static str = "55e6b5a0ae23878dc10c644a613d7072de861110";

const USE_DHCP: bool = false;
const UDP_AND_TCP_PORT: u16 = 8000;
const PRINT_PACKET_STATS: bool = false;
const NUM_RX_SLOTS: usize = 16;
const NUM_TX_SLOTS: usize = 16;

const STATIC_IPV4_CONFIG: StaticConfigV4 = StaticConfigV4 {
    address: Ipv4Cidr::new(Ipv4Addr::new(10, 0, 0, 25), 24),
    gateway: None,
    dns_servers: heapless::Vec::new(),
};

const INIT_STRING: &str = "-- Zynq 7000 Zedboard Ethernet Example --\n\r";

// Unicast address with OUI of the Marvell 88E1518 PHY.
const MAC_ADDRESS: [u8; 6] = [
    0x00,
    0x12,
    0x34,
    0x00,
    0x00,
    0x01,
];

/// See memory.x file. 1 MB starting at this address will be configured as uncached memory using the
/// MMU.
const UNCACHED_ADDR: u32 = 0x4000000;

// These descriptors must be placed in uncached memory. The MMU will be used to configure the
// .uncached memory segment as device memory.
#[unsafe(link_section = ".uncached")]
static RX_DESCRIPTORS: zynq7000_hal::eth::rx_descr::DescriptorList<NUM_RX_SLOTS> =
    zynq7000_hal::eth::rx_descr::DescriptorList::new();
#[unsafe(link_section = ".uncached")]
static TX_DESCRIPTORS: zynq7000_hal::eth::tx_descr::DescriptorList<NUM_TX_SLOTS> =
    zynq7000_hal::eth::tx_descr::DescriptorList::new();

static ETH_ERR_QUEUE: embassy_sync::channel::Channel<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    InterruptResult,
    8,
> = embassy_sync::channel::Channel::new();

#[derive(Debug, PartialEq, Eq)]
pub enum IpMode {
    LinkDown,
    AutoNegotiating,
    AwaitingIpConfig,
    StackReady,
}

/// Entry point which calls the embassy main method.
#[zynq7000_rt::entry]
fn entry_point() -> ! {
    main();
}

#[embassy_executor::task]
async fn embassy_net_task(
    mut runner: embassy_net::Runner<'static, zynq7000_hal::eth::embassy_net::Driver>,
) -> ! {
    runner.run().await
}

/// Simple UDP echo task.
#[embassy_executor::task]
async fn udp_task(mut udp: UdpSocket<'static>) -> ! {
    let mut rx_buf = [0; zynq7000_hal::eth::MTU];
    udp.bind(UDP_AND_TCP_PORT)
        .expect("failed to bind UDP socket to port 8000");
    loop {
            udp.send_to_with(
                1472,
                embassy_net::IpEndpoint::new(embassy_net::IpAddress::v4(10, 0, 0, 1), 1735),
                |buf| (1472, ()),
            )
            .await;
            continue;
        match udp.recv_from(&mut rx_buf).await {
            Ok((data, meta)) => {
                match udp.send_to(&rx_buf[0..data], meta).await {
                    Ok(_) => (),
                    Err(e) => {
                        Timer::after_millis(100).await;
                    }
                }
            }
            Err(e) => {
                Timer::after_millis(100).await;
            }
        }
    }
}

/// Simple TCP echo task.
#[embassy_executor::task]
async fn tcp_task(mut tcp: TcpSocket<'static>) -> ! {
    let mut rx_buf = [0; zynq7000_hal::eth::MTU];
    tcp.set_timeout(Some(Duration::from_secs(2)));
    loop {
        match tcp.accept(UDP_AND_TCP_PORT).await {
            Ok(_) => {
                defmt::info!("tcp connection to {:?} accepted", tcp.remote_endpoint());
                loop {
                    if tcp.may_recv() {
                        match tcp.read(&mut rx_buf).await {
                            Ok(0) => {
                                defmt::info!("tcp EOF received");
                                tcp.close();
                            }
                            Ok(read_bytes) => {
                                //defmt::info!("tcp rx {read_bytes} bytes");
                                if tcp.may_send() {
                                    match tcp.write_all(&rx_buf[0..read_bytes]).await {
                                        Ok(_) => continue,
                                        Err(e) => {
                                            Timer::after_millis(100).await;
                                        }
                                    }
                                } else {
                                    defmt::warn!("tcp remote endpoint not writeable");
                                    continue;
                                }
                            }
                            Err(_) => {
                                defmt::warn!("tcp connection reset by remote endpoint.");
                                tcp.close();
                            }
                        }
                    }
                    if !tcp.may_send() && !tcp.may_recv() {
                        defmt::info!("tcp send and receive side closed");
                        tcp.close();
                    }
                    if tcp.state() == embassy_net::tcp::State::Closed {
                        defmt::info!("tcp socket closed, exiting loop");
                        break;
                    }
                    Timer::after_millis(100).await;
                }
            }
            Err(e) => {
                Timer::after_millis(100).await;
                continue;
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    defmt::info!("starting");
    let mut dp = Peripherals::take().unwrap();
    l2_cache::init_with_defaults(&mut dp.l2c);

    // Enable PS-PL level shifters.
    configure_level_shifter(LevelShifterConfig::EnableAll);

    // Configure the uncached memory region using the MMU.
    mmu_l1_table_mut()
        .update(UNCACHED_ADDR, SHAREABLE_DEVICE)
        .expect("configuring uncached memory section failed");

    // Clock was already initialized by PS7 Init TCL script or FSBL, we just read it.
    let clocks = Clocks::new_from_regs(PS_CLOCK_FREQUENCY).unwrap();
    // Set up the global interrupt controller.
    let mut gic = Configurator::new_with_init(dp.gicc, dp.gicd);
    gic.enable_all_interrupts();
    gic.set_all_spi_interrupt_targets_cpu0();
    gic.enable();
    unsafe {
        gic.enable_interrupts();
    }

    // Set up global timer counter and embassy time driver.
    let gtc = GlobalTimerCounter::new(dp.gtc, clocks.arm_clocks());
    zynq7000_hal::time_driver_gtc::init(clocks.arm_clocks(), gtc);

    let boot_mode = BootMode::new_from_regs();
    //defmt::info!("Boot mode: {:?}", boot_mode);

    static ETH_RX_BUFS: static_cell::ConstStaticCell<[AlignedBuffer; NUM_RX_SLOTS]> =
        static_cell::ConstStaticCell::new(
            [AlignedBuffer([0; zynq7000_hal::eth::MTU]); NUM_RX_SLOTS],
        );
    static ETH_TX_BUFS: static_cell::ConstStaticCell<[AlignedBuffer; NUM_TX_SLOTS]> =
        static_cell::ConstStaticCell::new(
            [AlignedBuffer([0; zynq7000_hal::eth::MTU]); NUM_TX_SLOTS],
        );
    let rx_bufs = ETH_RX_BUFS.take();
    let tx_bufs = ETH_TX_BUFS.take();

    // Safety: We only call this once here.
    let rx_descr = unsafe { RX_DESCRIPTORS.take() };
    let tx_descr = unsafe { TX_DESCRIPTORS.take() };
    // Unwraps okay, list length is not 0
    let mut rx_descr_ref =
        zynq7000_hal::eth::rx_descr::DescriptorListWrapper::new(rx_descr.as_mut_slice());
    let mut tx_descr_ref =
        zynq7000_hal::eth::tx_descr::DescriptorListWrapper::new(tx_descr.as_mut_slice());
    rx_descr_ref.init_with_aligned_bufs(rx_bufs.as_slice());
    tx_descr_ref.init_or_reset();

    // Unwrap okay, this is a valid peripheral.
    let eth_ll = EthernetLowLevel::new(dp.eth_0).unwrap();
    let mod_id = eth_ll.regs.read_module_id();
    //info!("Ethernet Module ID: {mod_id:?}");
    assert_eq!(mod_id, 0x20118);

    let (clk_divs, clk_errors) = ClockDivSet::calculate_for_rgmii_and_io_clock(clocks.io_clocks());

    zynq7000_hal::register_interrupt(
        Interrupt::Spi(zynq7000_hal::gic::SpiInterrupt::Eth0),
        custom_eth_interupt_handler,
    );

    // Unwrap okay, we use a standard clock config, and the clock config should never fail.
    let eth_cfg = EthernetConfig::new(
        zynq7000_hal::eth::ClockConfig {
            src_sel: zynq7000::slcr::clocks::SrcSelIo::IoPll,
            use_emio_tx_clk: true,
            divs: clk_divs.cfg_1000_mbps,
            enable: true,
        },
        zynq7000_hal::eth::calculate_mdc_clk_div(clocks.arm_clocks()).unwrap(),
        MAC_ADDRESS,
    );
    // Configures all the physical pins for ethernet operation and sets up the
    // ethernet peripheral.
    let mut eth = zynq7000_hal::eth::Ethernet::new( eth_ll, eth_cfg, );

    eth.set_rx_buf_descriptor_base_address(rx_descr_ref.base_addr());
    eth.set_tx_buf_descriptor_base_address(tx_descr_ref.base_addr());
    eth.start();

    let driver = zynq7000_hal::eth::embassy_net::Driver::new(
        &eth,
        MAC_ADDRESS,
        zynq7000_hal::eth::embassy_net::DescriptorsAndBuffers::new(
            rx_descr_ref,
            rx_bufs,
            tx_descr_ref,
            tx_bufs,
        )
        .unwrap(),
    );
    let config = if USE_DHCP {
        embassy_net::Config::dhcpv4(Default::default())
    } else {
        embassy_net::Config::ipv4_static(STATIC_IPV4_CONFIG)
    };
    static RESOURCES: static_cell::StaticCell<embassy_net::StackResources<3>> =
        static_cell::StaticCell::new();
    let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
    let (stack, runner) = embassy_net::new(
        driver,
        config,
        RESOURCES.init(embassy_net::StackResources::new()),
        rng.next_u64(),
    );

    const N_SLOTS: usize = 6;
    const BUFSIZE: usize = 16 * 1024;

    // Ensure those are in the data section by making them static.
    static RX_UDP_META: static_cell::ConstStaticCell<[embassy_net::udp::PacketMetadata; N_SLOTS]> =
        static_cell::ConstStaticCell::new([embassy_net::udp::PacketMetadata::EMPTY; N_SLOTS]);
    static TX_UDP_META: static_cell::ConstStaticCell<[embassy_net::udp::PacketMetadata; N_SLOTS]> =
        static_cell::ConstStaticCell::new([embassy_net::udp::PacketMetadata::EMPTY; N_SLOTS]);
    static TX_UDP_BUFS: static_cell::ConstStaticCell<[u8; BUFSIZE]> =
        static_cell::ConstStaticCell::new([0; BUFSIZE]);
    static RX_UDP_BUFS: static_cell::ConstStaticCell<[u8; BUFSIZE]> =
        static_cell::ConstStaticCell::new([0; BUFSIZE]);

    let udp_socket = UdpSocket::new(
        stack,
        RX_UDP_META.take(),
        RX_UDP_BUFS.take(),
        TX_UDP_META.take(),
        TX_UDP_BUFS.take(),
    );

    // Ensure those are in the data section by making them static.
    static TX_TCP_BUFS: static_cell::ConstStaticCell<[u8; zynq7000_hal::eth::MTU]> =
        static_cell::ConstStaticCell::new([0; zynq7000_hal::eth::MTU]);
    static RX_TCP_BUFS: static_cell::ConstStaticCell<[u8; zynq7000_hal::eth::MTU]> =
        static_cell::ConstStaticCell::new([0; zynq7000_hal::eth::MTU]);

    let tcp_socket = TcpSocket::new(stack, RX_TCP_BUFS.take(), TX_TCP_BUFS.take());

    // Spawn all embassy tasks.
    spawner.spawn(embassy_net_task(runner).unwrap());
    spawner.spawn(udp_task(udp_socket).unwrap());
    spawner.spawn(tcp_task(tcp_socket).unwrap());

    let mut ip_mode = IpMode::LinkDown;
    let mut transmitted_frames = 0;
    let mut received_frames = 0;
    let receiver = ETH_ERR_QUEUE.receiver();
    loop {
        // Handle error messages from ethernet interrupt.
        while let Ok(msg) = receiver.try_receive() {
        }
        if PRINT_PACKET_STATS {
            let sent_frames_since_last = eth.ll().regs.statistics().read_tx_count();
            if sent_frames_since_last > 0 {
                transmitted_frames += sent_frames_since_last;
            }
            let received_frames_since_last = eth.ll().regs.statistics().read_rx_count();
            if received_frames_since_last > 0 {
                received_frames += received_frames_since_last;
            }
        }

        // This is basically a linker checker task. It also takes care of notifying the
        // embassy stack of link state changes.
        match ip_mode {
            // Assuming that auto-negotiation is performed automatically.
            IpMode::LinkDown => {
                //mio_led.set_low();
                zynq7000_hal::eth::embassy_net::update_link_state(
                    embassy_net::driver::LinkState::Down,
                );
                ip_mode = IpMode::AutoNegotiating;
            }
            IpMode::AutoNegotiating => {
                eth.configure_clock_and_speed_duplex(Speed::Mbps1000, Duplex::Full, &clk_divs);
                zynq7000_hal::eth::embassy_net::update_link_state(
                    embassy_net::driver::LinkState::Up,
                );
                ip_mode = IpMode::AwaitingIpConfig;
            }
            IpMode::AwaitingIpConfig => {
                if stack.is_config_up() {
                    let network_config = stack.config_v4();
                    defmt::info!("Network configuration is up");
                    ip_mode = IpMode::StackReady;
                } else {
                    Timer::after_millis(100).await;
                }
            }
            IpMode::StackReady => {
                Timer::after_millis(100).await;
            }
        }
    }
}

// Safety: Only called by interrupt handler, registered in global interrupt handler map.
unsafe fn custom_eth_interupt_handler() {
    // This generic library provided interrupt handler takes care of waking
    // the driver on received or sent frames while also reporting anomalies
    // and errors.
    let result = zynq7000_hal::eth::embassy_net::on_interrupt(zynq7000_hal::eth::EthernetId::Eth0);
    if result.has_errors() {
        ETH_ERR_QUEUE.try_send(result).ok();
    }
}

#[zynq7000_rt::irq]
pub fn irq_handler() {
    // Safety: Called here once.
    let result = unsafe { generic_interrupt_handler() };
    if let Err(_) = result {
        defmt::error!("Generic interrupt handler failed handling");
    }
}

#[zynq7000_rt::exception(DataAbort)]
fn data_abort_handler(_faulting_addr: usize) -> ! {
    loop {
        nop();
    }
}

#[zynq7000_rt::exception(Undefined)]
fn undefined_handler(_faulting_addr: usize) -> ! {
    loop {
        nop();
    }
}

#[zynq7000_rt::exception(PrefetchAbort)]
fn prefetch_handler(_faulting_addr: usize) -> ! {
    loop {
        nop();
    }
}

/// Panic handler
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    defmt::error!("Panic: {:?}", info);
    loop {}
}

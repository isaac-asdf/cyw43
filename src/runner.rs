use core::slice;

use embassy_futures::select::{select3, Either3};
use embassy_net_driver_channel as ch;
use embassy_sync::pubsub::PubSubBehavior;
use embassy_time::{block_for, Duration, Timer};
use embedded_hal_1::digital::OutputPin;

use crate::bus::Bus;
pub use crate::bus::SpiBusCyw43;
use crate::consts::*;
use crate::events::{EventQueue, EventStatus};
use crate::fmt::Bytes;
use crate::ioctl::{IoctlState, IoctlType, PendingIoctl};
use crate::nvram::NVRAM;
use crate::structs::*;
use crate::{events, Core, CHIP, MTU};

#[cfg(feature = "firmware-logs")]
struct LogState {
    addr: u32,
    last_idx: usize,
    buf: [u8; 256],
    buf_count: usize,
}

#[cfg(feature = "firmware-logs")]
impl Default for LogState {
    fn default() -> Self {
        Self {
            addr: Default::default(),
            last_idx: Default::default(),
            buf: [0; 256],
            buf_count: Default::default(),
        }
    }
}

pub struct Runner<'a, PWR, SPI> {
    ch: ch::Runner<'a, MTU>,
    bus: Bus<PWR, SPI>,

    ioctl_state: &'a IoctlState,
    ioctl_id: u16,
    sdpcm_seq: u8,
    sdpcm_seq_max: u8,

    events: &'a EventQueue,

    #[cfg(feature = "firmware-logs")]
    log: LogState,
}

impl<'a, PWR, SPI> Runner<'a, PWR, SPI>
where
    PWR: OutputPin,
    SPI: SpiBusCyw43,
{
    pub(crate) fn new(
        ch: ch::Runner<'a, MTU>,
        bus: Bus<PWR, SPI>,
        ioctl_state: &'a IoctlState,
        events: &'a EventQueue,
    ) -> Self {
        Self {
            ch,
            bus,
            ioctl_state,
            ioctl_id: 0,
            sdpcm_seq: 0,
            sdpcm_seq_max: 1,
            events,
            #[cfg(feature = "firmware-logs")]
            log: LogState::default(),
        }
    }

    pub(crate) async fn init(&mut self, firmware: &[u8]) {
        self.bus.init().await;

        // Init ALP (Active Low Power) clock
        self.bus
            .write8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR, BACKPLANE_ALP_AVAIL_REQ)
            .await;
        info!("waiting for clock...");
        while self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR).await & BACKPLANE_ALP_AVAIL == 0 {}
        info!("clock ok");

        let chip_id = self.bus.bp_read16(0x1800_0000).await;
        info!("chip ID: {}", chip_id);

        // Upload firmware.
        self.core_disable(Core::WLAN).await;
        self.core_reset(Core::SOCSRAM).await;
        self.bus.bp_write32(CHIP.socsram_base_address + 0x10, 3).await;
        self.bus.bp_write32(CHIP.socsram_base_address + 0x44, 0).await;

        let ram_addr = CHIP.atcm_ram_base_address;

        info!("loading fw");
        self.bus.bp_write(ram_addr, firmware).await;

        info!("loading nvram");
        // Round up to 4 bytes.
        let nvram_len = (NVRAM.len() + 3) / 4 * 4;
        self.bus
            .bp_write(ram_addr + CHIP.chip_ram_size - 4 - nvram_len as u32, NVRAM)
            .await;

        let nvram_len_words = nvram_len as u32 / 4;
        let nvram_len_magic = (!nvram_len_words << 16) | nvram_len_words;
        self.bus
            .bp_write32(ram_addr + CHIP.chip_ram_size - 4, nvram_len_magic)
            .await;

        // Start core!
        info!("starting up core...");
        self.core_reset(Core::WLAN).await;
        assert!(self.core_is_up(Core::WLAN).await);

        while self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR).await & 0x80 == 0 {}

        // "Set up the interrupt mask and enable interrupts"
        // self.bus.bp_write32(CHIP.sdiod_core_base_address + 0x24, 0xF0).await;

        self.bus
            .write16(FUNC_BUS, REG_BUS_INTERRUPT_ENABLE, IRQ_F2_PACKET_AVAILABLE)
            .await;

        // "Lower F2 Watermark to avoid DMA Hang in F2 when SD Clock is stopped."
        // Sounds scary...
        self.bus
            .write8(FUNC_BACKPLANE, REG_BACKPLANE_FUNCTION2_WATERMARK, 32)
            .await;

        // wait for wifi startup
        info!("waiting for wifi init...");
        while self.bus.read32(FUNC_BUS, REG_BUS_STATUS).await & STATUS_F2_RX_READY == 0 {}

        // Some random configs related to sleep.
        // These aren't needed if we don't want to sleep the bus.
        // TODO do we need to sleep the bus to read the irq line, due to
        // being on the same pin as MOSI/MISO?

        /*
        let mut val = self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_WAKEUP_CTRL).await;
        val |= 0x02; // WAKE_TILL_HT_AVAIL
        self.bus.write8(FUNC_BACKPLANE, REG_BACKPLANE_WAKEUP_CTRL, val).await;
        self.bus.write8(FUNC_BUS, 0xF0, 0x08).await; // SDIOD_CCCR_BRCM_CARDCAP.CMD_NODEC = 1
        self.bus.write8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR, 0x02).await; // SBSDIO_FORCE_HT

        let mut val = self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_SLEEP_CSR).await;
        val |= 0x01; // SBSDIO_SLPCSR_KEEP_SDIO_ON
        self.bus.write8(FUNC_BACKPLANE, REG_BACKPLANE_SLEEP_CSR, val).await;
         */

        // clear pulls
        self.bus.write8(FUNC_BACKPLANE, REG_BACKPLANE_PULL_UP, 0).await;
        let _ = self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_PULL_UP).await;

        // start HT clock
        //self.bus.write8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR, 0x10).await;
        //info!("waiting for HT clock...");
        //while self.bus.read8(FUNC_BACKPLANE, REG_BACKPLANE_CHIP_CLOCK_CSR).await & 0x80 == 0 {}
        //info!("clock ok");

        #[cfg(feature = "firmware-logs")]
        self.log_init().await;

        info!("init done ");
    }

    #[cfg(feature = "firmware-logs")]
    async fn log_init(&mut self) {
        // Initialize shared memory for logging.

        let addr = CHIP.atcm_ram_base_address + CHIP.chip_ram_size - 4 - CHIP.socram_srmem_size;
        let shared_addr = self.bus.bp_read32(addr).await;
        info!("shared_addr {:08x}", shared_addr);

        let mut shared = [0; SharedMemData::SIZE];
        self.bus.bp_read(shared_addr, &mut shared).await;
        let shared = SharedMemData::from_bytes(&shared);

        self.log.addr = shared.console_addr + 8;
    }

    #[cfg(feature = "firmware-logs")]
    async fn log_read(&mut self) {
        // Read log struct
        let mut log = [0; SharedMemLog::SIZE];
        self.bus.bp_read(self.log.addr, &mut log).await;
        let log = SharedMemLog::from_bytes(&log);

        let idx = log.idx as usize;

        // If pointer hasn't moved, no need to do anything.
        if idx == self.log.last_idx {
            return;
        }

        // Read entire buf for now. We could read only what we need, but then we
        // run into annoying alignment issues in `bp_read`.
        let mut buf = [0; 0x400];
        self.bus.bp_read(log.buf, &mut buf).await;

        while self.log.last_idx != idx as usize {
            let b = buf[self.log.last_idx];
            if b == b'\r' || b == b'\n' {
                if self.log.buf_count != 0 {
                    let s = unsafe { core::str::from_utf8_unchecked(&self.log.buf[..self.log.buf_count]) };
                    debug!("LOGS: {}", s);
                    self.log.buf_count = 0;
                }
            } else if self.log.buf_count < self.log.buf.len() {
                self.log.buf[self.log.buf_count] = b;
                self.log.buf_count += 1;
            }

            self.log.last_idx += 1;
            if self.log.last_idx == 0x400 {
                self.log.last_idx = 0;
            }
        }
    }

    pub async fn run(mut self) -> ! {
        let mut buf = [0; 512];
        loop {
            #[cfg(feature = "firmware-logs")]
            self.log_read().await;

            if self.has_credit() {
                let ioctl = self.ioctl_state.wait_pending();
                let tx = self.ch.tx_buf();
                let ev = self.bus.wait_for_event();

                match select3(ioctl, tx, ev).await {
                    Either3::First(PendingIoctl {
                        buf: iobuf,
                        kind,
                        cmd,
                        iface,
                    }) => {
                        self.send_ioctl(kind, cmd, iface, unsafe { &*iobuf }).await;
                        self.check_status(&mut buf).await;
                    }
                    Either3::Second(packet) => {
                        trace!("tx pkt {:02x}", Bytes(&packet[..packet.len().min(48)]));

                        let mut buf = [0; 512];
                        let buf8 = slice8_mut(&mut buf);

                        let total_len = SdpcmHeader::SIZE + BcdHeader::SIZE + packet.len();

                        let seq = self.sdpcm_seq;
                        self.sdpcm_seq = self.sdpcm_seq.wrapping_add(1);

                        let sdpcm_header = SdpcmHeader {
                            len: total_len as u16, // TODO does this len need to be rounded up to u32?
                            len_inv: !total_len as u16,
                            sequence: seq,
                            channel_and_flags: CHANNEL_TYPE_DATA,
                            next_length: 0,
                            header_length: SdpcmHeader::SIZE as _,
                            wireless_flow_control: 0,
                            bus_data_credit: 0,
                            reserved: [0, 0],
                        };

                        let bcd_header = BcdHeader {
                            flags: BDC_VERSION << BDC_VERSION_SHIFT,
                            priority: 0,
                            flags2: 0,
                            data_offset: 0,
                        };
                        trace!("tx {:?}", sdpcm_header);
                        trace!("    {:?}", bcd_header);

                        buf8[0..SdpcmHeader::SIZE].copy_from_slice(&sdpcm_header.to_bytes());
                        buf8[SdpcmHeader::SIZE..][..BcdHeader::SIZE].copy_from_slice(&bcd_header.to_bytes());
                        buf8[SdpcmHeader::SIZE + BcdHeader::SIZE..][..packet.len()].copy_from_slice(packet);

                        let total_len = (total_len + 3) & !3; // round up to 4byte

                        trace!("    {:02x}", Bytes(&buf8[..total_len.min(48)]));

                        self.bus.wlan_write(&buf[..(total_len / 4)]).await;
                        self.ch.tx_done();
                        self.check_status(&mut buf).await;
                    }
                    Either3::Third(()) => {
                        self.handle_irq(&mut buf).await;
                    }
                }
            } else {
                warn!("TX stalled");
                self.bus.wait_for_event().await;
                self.handle_irq(&mut buf).await;
            }
        }
    }

    /// Wait for IRQ on F2 packet available
    async fn handle_irq(&mut self, buf: &mut [u32; 512]) {
        // Receive stuff
        let irq = self.bus.read16(FUNC_BUS, REG_BUS_INTERRUPT).await;
        trace!("irq{}", FormatInterrupt(irq));

        if irq & IRQ_F2_PACKET_AVAILABLE != 0 {
            self.check_status(buf).await;
        }

        if irq & IRQ_DATA_UNAVAILABLE != 0 {
            // TODO what should we do here?
            warn!("IRQ DATA_UNAVAILABLE, clearing...");
            self.bus.write16(FUNC_BUS, REG_BUS_INTERRUPT, 1).await;
        }
    }

    /// Handle F2 events while status register is set
    async fn check_status(&mut self, buf: &mut [u32; 512]) {
        loop {
            let status = self.bus.status();
            trace!("check status{}", FormatStatus(status));

            if status & STATUS_F2_PKT_AVAILABLE != 0 {
                let len = (status & STATUS_F2_PKT_LEN_MASK) >> STATUS_F2_PKT_LEN_SHIFT;
                self.bus.wlan_read(buf, len).await;
                trace!("rx {:02x}", Bytes(&slice8_mut(buf)[..(len as usize).min(48)]));
                self.rx(&slice8_mut(buf)[..len as usize]);
            } else {
                break;
            }
        }
    }

    fn rx(&mut self, packet: &[u8]) {
        if packet.len() < SdpcmHeader::SIZE {
            warn!("packet too short, len={}", packet.len());
            return;
        }

        let sdpcm_header = SdpcmHeader::from_bytes(packet[..SdpcmHeader::SIZE].try_into().unwrap());
        trace!("rx {:?}", sdpcm_header);
        if sdpcm_header.len != !sdpcm_header.len_inv {
            warn!("len inv mismatch");
            return;
        }
        if sdpcm_header.len as usize != packet.len() {
            // TODO: is this guaranteed??
            warn!("len from header doesn't match len from spi");
            return;
        }

        self.update_credit(&sdpcm_header);

        let channel = sdpcm_header.channel_and_flags & 0x0f;

        let payload = &packet[sdpcm_header.header_length as _..];

        match channel {
            CHANNEL_TYPE_CONTROL => {
                if payload.len() < CdcHeader::SIZE {
                    warn!("payload too short, len={}", payload.len());
                    return;
                }

                let cdc_header = CdcHeader::from_bytes(payload[..CdcHeader::SIZE].try_into().unwrap());
                trace!("    {:?}", cdc_header);

                if cdc_header.id == self.ioctl_id {
                    if cdc_header.status != 0 {
                        // TODO: propagate error instead
                        panic!("IOCTL error {}", cdc_header.status as i32);
                    }

                    let resp_len = cdc_header.len as usize;
                    let response = &payload[CdcHeader::SIZE..][..resp_len];
                    info!("IOCTL Response: {:02x}", Bytes(response));

                    self.ioctl_state.ioctl_done(response);
                }
            }
            CHANNEL_TYPE_EVENT => {
                let bcd_header = BcdHeader::from_bytes(&payload[..BcdHeader::SIZE].try_into().unwrap());
                trace!("    {:?}", bcd_header);

                let packet_start = BcdHeader::SIZE + 4 * bcd_header.data_offset as usize;

                if packet_start + EventPacket::SIZE > payload.len() {
                    warn!("BCD event, incomplete header");
                    return;
                }
                let bcd_packet = &payload[packet_start..];
                trace!("    {:02x}", Bytes(&bcd_packet[..(bcd_packet.len() as usize).min(36)]));

                let mut event_packet = EventPacket::from_bytes(&bcd_packet[..EventPacket::SIZE].try_into().unwrap());
                event_packet.byteswap();

                const ETH_P_LINK_CTL: u16 = 0x886c; // HPNA, wlan link local tunnel, according to linux if_ether.h
                if event_packet.eth.ether_type != ETH_P_LINK_CTL {
                    warn!(
                        "unexpected ethernet type 0x{:04x}, expected Broadcom ether type 0x{:04x}",
                        event_packet.eth.ether_type, ETH_P_LINK_CTL
                    );
                    return;
                }
                const BROADCOM_OUI: &[u8] = &[0x00, 0x10, 0x18];
                if event_packet.hdr.oui != BROADCOM_OUI {
                    warn!(
                        "unexpected ethernet OUI {:02x}, expected Broadcom OUI {:02x}",
                        Bytes(&event_packet.hdr.oui),
                        Bytes(BROADCOM_OUI)
                    );
                    return;
                }
                const BCMILCP_SUBTYPE_VENDOR_LONG: u16 = 32769;
                if event_packet.hdr.subtype != BCMILCP_SUBTYPE_VENDOR_LONG {
                    warn!("unexpected subtype {}", event_packet.hdr.subtype);
                    return;
                }

                const BCMILCP_BCM_SUBTYPE_EVENT: u16 = 1;
                if event_packet.hdr.user_subtype != BCMILCP_BCM_SUBTYPE_EVENT {
                    warn!("unexpected user_subtype {}", event_packet.hdr.subtype);
                    return;
                }

                if event_packet.msg.datalen as usize >= (bcd_packet.len() - EventMessage::SIZE) {
                    warn!("BCD event, incomplete data");
                    return;
                }

                let evt_type = events::Event::from(event_packet.msg.event_type as u8);
                let evt_data = &bcd_packet[EventMessage::SIZE..][..event_packet.msg.datalen as usize];
                debug!(
                    "=== EVENT {:?}: {:?} {:02x}",
                    evt_type,
                    event_packet.msg,
                    Bytes(evt_data)
                );

                if evt_type == events::Event::AUTH || evt_type == events::Event::JOIN {
                    self.events.publish_immediate(EventStatus {
                        status: event_packet.msg.status,
                        event_type: evt_type,
                    });
                }
            }
            CHANNEL_TYPE_DATA => {
                let bcd_header = BcdHeader::from_bytes(&payload[..BcdHeader::SIZE].try_into().unwrap());
                trace!("    {:?}", bcd_header);

                let packet_start = BcdHeader::SIZE + 4 * bcd_header.data_offset as usize;
                if packet_start > payload.len() {
                    warn!("packet start out of range.");
                    return;
                }
                let packet = &payload[packet_start..];
                trace!("rx pkt {:02x}", Bytes(&packet[..(packet.len() as usize).min(48)]));

                match self.ch.try_rx_buf() {
                    Some(buf) => {
                        buf[..packet.len()].copy_from_slice(packet);
                        self.ch.rx_done(packet.len())
                    }
                    None => warn!("failed to push rxd packet to the channel."),
                }
            }
            _ => {}
        }
    }

    fn update_credit(&mut self, sdpcm_header: &SdpcmHeader) {
        if sdpcm_header.channel_and_flags & 0xf < 3 {
            let mut sdpcm_seq_max = sdpcm_header.bus_data_credit;
            if sdpcm_seq_max.wrapping_sub(self.sdpcm_seq) > 0x40 {
                sdpcm_seq_max = self.sdpcm_seq + 2;
            }
            self.sdpcm_seq_max = sdpcm_seq_max;
        }
    }

    fn has_credit(&self) -> bool {
        self.sdpcm_seq != self.sdpcm_seq_max && self.sdpcm_seq_max.wrapping_sub(self.sdpcm_seq) & 0x80 == 0
    }

    async fn send_ioctl(&mut self, kind: IoctlType, cmd: u32, iface: u32, data: &[u8]) {
        let mut buf = [0; 512];
        let buf8 = slice8_mut(&mut buf);

        let total_len = SdpcmHeader::SIZE + CdcHeader::SIZE + data.len();

        let sdpcm_seq = self.sdpcm_seq;
        self.sdpcm_seq = self.sdpcm_seq.wrapping_add(1);
        self.ioctl_id = self.ioctl_id.wrapping_add(1);

        let sdpcm_header = SdpcmHeader {
            len: total_len as u16, // TODO does this len need to be rounded up to u32?
            len_inv: !total_len as u16,
            sequence: sdpcm_seq,
            channel_and_flags: CHANNEL_TYPE_CONTROL,
            next_length: 0,
            header_length: SdpcmHeader::SIZE as _,
            wireless_flow_control: 0,
            bus_data_credit: 0,
            reserved: [0, 0],
        };

        let cdc_header = CdcHeader {
            cmd: cmd,
            len: data.len() as _,
            flags: kind as u16 | (iface as u16) << 12,
            id: self.ioctl_id,
            status: 0,
        };
        trace!("tx {:?}", sdpcm_header);
        trace!("    {:?}", cdc_header);

        buf8[0..SdpcmHeader::SIZE].copy_from_slice(&sdpcm_header.to_bytes());
        buf8[SdpcmHeader::SIZE..][..CdcHeader::SIZE].copy_from_slice(&cdc_header.to_bytes());
        buf8[SdpcmHeader::SIZE + CdcHeader::SIZE..][..data.len()].copy_from_slice(data);

        let total_len = (total_len + 3) & !3; // round up to 4byte

        trace!("    {:02x}", Bytes(&buf8[..total_len.min(48)]));

        self.bus.wlan_write(&buf[..total_len / 4]).await;
    }

    async fn core_disable(&mut self, core: Core) {
        let base = core.base_addr();

        // Dummy read?
        let _ = self.bus.bp_read8(base + AI_RESETCTRL_OFFSET).await;

        // Check it isn't already reset
        let r = self.bus.bp_read8(base + AI_RESETCTRL_OFFSET).await;
        if r & AI_RESETCTRL_BIT_RESET != 0 {
            return;
        }

        self.bus.bp_write8(base + AI_IOCTRL_OFFSET, 0).await;
        let _ = self.bus.bp_read8(base + AI_IOCTRL_OFFSET).await;

        block_for(Duration::from_millis(1));

        self.bus
            .bp_write8(base + AI_RESETCTRL_OFFSET, AI_RESETCTRL_BIT_RESET)
            .await;
        let _ = self.bus.bp_read8(base + AI_RESETCTRL_OFFSET).await;
    }

    async fn core_reset(&mut self, core: Core) {
        self.core_disable(core).await;

        let base = core.base_addr();
        self.bus
            .bp_write8(base + AI_IOCTRL_OFFSET, AI_IOCTRL_BIT_FGC | AI_IOCTRL_BIT_CLOCK_EN)
            .await;
        let _ = self.bus.bp_read8(base + AI_IOCTRL_OFFSET).await;

        self.bus.bp_write8(base + AI_RESETCTRL_OFFSET, 0).await;

        Timer::after(Duration::from_millis(1)).await;

        self.bus
            .bp_write8(base + AI_IOCTRL_OFFSET, AI_IOCTRL_BIT_CLOCK_EN)
            .await;
        let _ = self.bus.bp_read8(base + AI_IOCTRL_OFFSET).await;

        Timer::after(Duration::from_millis(1)).await;
    }

    async fn core_is_up(&mut self, core: Core) -> bool {
        let base = core.base_addr();

        let io = self.bus.bp_read8(base + AI_IOCTRL_OFFSET).await;
        if io & (AI_IOCTRL_BIT_FGC | AI_IOCTRL_BIT_CLOCK_EN) != AI_IOCTRL_BIT_CLOCK_EN {
            debug!("core_is_up: returning false due to bad ioctrl {:02x}", io);
            return false;
        }

        let r = self.bus.bp_read8(base + AI_RESETCTRL_OFFSET).await;
        if r & (AI_RESETCTRL_BIT_RESET) != 0 {
            debug!("core_is_up: returning false due to bad resetctrl {:02x}", r);
            return false;
        }

        true
    }
}

fn slice8_mut(x: &mut [u32]) -> &mut [u8] {
    let len = x.len() * 4;
    unsafe { slice::from_raw_parts_mut(x.as_mut_ptr() as _, len) }
}

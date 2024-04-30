#![macro_use]

use core::future::Future;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{fence, Ordering};
use core::task::{Context, Poll};

use embassy_hal_internal::{into_ref, Peripheral, PeripheralRef};
use embassy_sync::waitqueue::AtomicWaker;

use super::word::{Word, WordSize};
use super::{AnyChannel, Channel, Dir, Request, STATE};
use crate::interrupt::typelevel::Interrupt;
use crate::interrupt::Priority;
use crate::pac;
use crate::pac::gpdma::vals;

pub(crate) struct ChannelInfo {
    pub(crate) dma: pac::gpdma::Gpdma,
    pub(crate) num: usize,
}

/// GPDMA transfer options.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct TransferOptions {}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {}
    }
}

impl From<WordSize> for vals::Dw {
    fn from(raw: WordSize) -> Self {
        match raw {
            WordSize::OneByte => Self::BYTE,
            WordSize::TwoBytes => Self::HALFWORD,
            WordSize::FourBytes => Self::WORD,
        }
    }
}

pub(crate) struct ChannelState {
    waker: AtomicWaker,
}

impl ChannelState {
    pub(crate) const NEW: Self = Self {
        waker: AtomicWaker::new(),
    };
}

/// safety: must be called only once
pub(crate) unsafe fn init(cs: critical_section::CriticalSection, irq_priority: Priority) {
    foreach_interrupt! {
        ($peri:ident, gpdma, $block:ident, $signal_name:ident, $irq:ident) => {
            crate::interrupt::typelevel::$irq::set_priority_with_cs(cs, irq_priority);
            crate::interrupt::typelevel::$irq::enable();
        };
    }
    crate::_generated::init_gpdma();
}

impl AnyChannel {
    /// Safety: Must be called with a matching set of parameters for a valid dma channel
    pub(crate) unsafe fn on_irq(&self) {
        let info = self.info();
        let state = &STATE[self.id as usize];

        let ch = info.dma.ch(info.num);
        let sr = ch.sr().read();

        if sr.dtef() {
            panic!(
                "DMA: data transfer error on DMA@{:08x} channel {}",
                info.dma.as_ptr() as u32,
                info.num
            );
        }
        if sr.usef() {
            panic!(
                "DMA: user settings error on DMA@{:08x} channel {}",
                info.dma.as_ptr() as u32,
                info.num
            );
        }

        if sr.suspf() || sr.tcf() {
            // disable all xxIEs to prevent the irq from firing again.
            ch.cr().write(|_| {});

            // Wake the future. It'll look at tcf and see it's set.
            state.waker.wake();
        }
    }
}

/// Linked List Table
#[derive(Debug)]
pub struct LliTable<'a, W: Word, const MEMS: usize, const BUFLEN: usize> {
    addrs: &'a [&'a [W; BUFLEN]; MEMS],
    items: [LliItem; MEMS],
    size: WordSize,
}

impl<'a, W: Word, const MEMS: usize, const BUFLEN: usize> LliTable<'a, W, MEMS, BUFLEN> {
    /// sets the buffer addresses
    ///
    /// let buf0 = [0u16; 6];
    /// let buf1 = [0u16; 6];
    /// let bufs = [&buf0,&buf1];
    /// let lli_table = LliTable::new(&bufs);
    ///
    /// // after move or copy use fixing_in_mem(LliOption)
    ///
    pub fn new(addrs: &'a [&'a [W; BUFLEN]; MEMS]) -> Self {
        assert!(MEMS > 1);
        assert!(BUFLEN > 0);
        let mut items = [LliItem::default(); MEMS];
        //map the buffer startaddr to lli
        for (index, buf) in addrs.into_iter().enumerate() {
            let (ptr, mut len) = super::slice_ptr_parts(*buf);
            len *= W::size().bytes();
            assert!(len > 0 && len <= 0xFFFF);
            items[index].dar = ptr as u32;
        }
        Self {
            items,
            size: W::size(),
            addrs,
        }
    }

    /// Create the Linked List
    /// use it after a copy or move
    pub fn fixing_in_mem(&mut self, option: LliOption) {
        // create linked list
        for i in 0..MEMS - 1 {
            let lli_plus_one = ptr::addr_of!(self.items[i + 1]) as u16;
            let lli = &mut self.items[i];
            lli.set_llr(lli_plus_one);
        }
        match option {
            LliOption::Repeated => self.items[MEMS - 1].set_llr(ptr::addr_of!(self.items[0]) as u16), // Connect the end and the beginning
            LliOption::Single => self.items[MEMS - 1].llr = 0,
        }
    }

    /// the memory width
    pub fn get_size(&self) -> WordSize {
        self.size
    }

    /// get the last address from the buffer for double Buffer mode
    /// Buffer 0 and 1 must not be the same
    pub fn find_last_buffer_in_double_buffer_mode(&self, dar: u32) -> &[W; BUFLEN] {
        assert!(MEMS == 2);
        match self.items[0].dar == dar {
            true => self.addrs[1],
            _ => self.addrs[0],
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
/// Linked List Item
pub struct LliItem {
    /// Data Start Address
    pub dar: u32,
    /// Linked List
    pub llr: u32,
}
#[allow(unused)]
impl LliItem {
    fn set_llr(&mut self, la: u16) {
        // set la, uda and ull
        self.llr = (la as u32) | 1u32 << 27 | 1u32 << 16;
    }
}

/// Definition for the end of the linked list
pub enum LliOption {
    /// The end of the list is linked to the beginning
    Repeated,
    /// the list is only processed once
    Single,
}

/// DMA transfer.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Transfer<'a> {
    channel: PeripheralRef<'a, AnyChannel>,
}

impl<'a> Transfer<'a> {
    /// Create a new read DMA transfer (peripheral to memory).
    pub unsafe fn new_read<W: Word>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        peri_addr: *mut W,
        buf: &'a mut [W],
        options: TransferOptions,
    ) -> Self {
        Self::new_read_raw(channel, request, peri_addr, buf, options)
    }

    /// Create a new read DMA transfer (peripheral to memory), using raw pointers.
    pub unsafe fn new_read_raw<W: Word>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        peri_addr: *mut W,
        buf: *mut [W],
        options: TransferOptions,
    ) -> Self {
        into_ref!(channel);

        let (ptr, len) = super::slice_ptr_parts_mut(buf);
        assert!(len > 0 && len <= 0xFFFF);

        Self::new_inner(
            channel.map_into(),
            request,
            Dir::PeripheralToMemory,
            peri_addr as *const u32,
            ptr as *mut u32,
            len,
            true,
            W::size(),
            options,
        )
    }

    /// Create a new write DMA transfer (memory to peripheral).
    pub unsafe fn new_write<W: Word>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        buf: &'a [W],
        peri_addr: *mut W,
        options: TransferOptions,
    ) -> Self {
        Self::new_write_raw(channel, request, buf, peri_addr, options)
    }

    /// Create a new write DMA transfer (memory to peripheral), using raw pointers.
    pub unsafe fn new_write_raw<W: Word>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        buf: *const [W],
        peri_addr: *mut W,
        options: TransferOptions,
    ) -> Self {
        into_ref!(channel);

        let (ptr, len) = super::slice_ptr_parts(buf);
        assert!(len > 0 && len <= 0xFFFF);

        Self::new_inner(
            channel.map_into(),
            request,
            Dir::MemoryToPeripheral,
            peri_addr as *const u32,
            ptr as *mut u32,
            len,
            true,
            W::size(),
            options,
        )
    }

    /// Create a new write DMA transfer (memory to peripheral), writing the same value repeatedly.
    pub unsafe fn new_write_repeated<W: Word>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        repeated: &'a W,
        count: usize,
        peri_addr: *mut W,
        options: TransferOptions,
    ) -> Self {
        into_ref!(channel);

        Self::new_inner(
            channel.map_into(),
            request,
            Dir::MemoryToPeripheral,
            peri_addr as *const u32,
            repeated as *const W as *mut u32,
            count,
            false,
            W::size(),
            options,
        )
    }

    unsafe fn new_inner(
        channel: PeripheralRef<'a, AnyChannel>,
        request: Request,
        dir: Dir,
        peri_addr: *const u32,
        mem_addr: *mut u32,
        mem_len: usize,
        incr_mem: bool,
        data_size: WordSize,
        _options: TransferOptions,
    ) -> Self {
        let info = channel.info();
        let ch = info.dma.ch(info.num);

        // "Preceding reads and writes cannot be moved past subsequent writes."
        fence(Ordering::SeqCst);

        let this = Self { channel };

        #[cfg(dmamux)]
        super::dmamux::configure_dmamux(&*this.channel, request);

        ch.cr().write(|w| w.set_reset(true));
        ch.fcr().write(|w| w.0 = 0xFFFF_FFFF); // clear all irqs
        ch.llr().write(|_| {}); // no linked list
        ch.tr1().write(|w| {
            w.set_sdw(data_size.into());
            w.set_ddw(data_size.into());
            w.set_sinc(dir == Dir::MemoryToPeripheral && incr_mem);
            w.set_dinc(dir == Dir::PeripheralToMemory && incr_mem);
        });
        ch.tr2().write(|w| {
            w.set_dreq(match dir {
                Dir::MemoryToPeripheral => vals::Dreq::DESTINATIONPERIPHERAL,
                Dir::PeripheralToMemory => vals::Dreq::SOURCEPERIPHERAL,
            });
            w.set_reqsel(request);
        });
        ch.br1().write(|w| {
            // BNDT is specified as bytes, not as number of transfers.
            w.set_bndt((mem_len * data_size.bytes()) as u16)
        });

        match dir {
            Dir::MemoryToPeripheral => {
                ch.sar().write_value(mem_addr as _);
                ch.dar().write_value(peri_addr as _);
            }
            Dir::PeripheralToMemory => {
                ch.sar().write_value(peri_addr as _);
                ch.dar().write_value(mem_addr as _);
            }
        }

        ch.cr().write(|w| {
            // Enable interrupts
            w.set_tcie(true);
            w.set_useie(true);
            w.set_dteie(true);
            w.set_suspie(true);

            // Start it
            w.set_en(true);
        });

        this
    }

    /// Create a new read DMA transfer (peripheral to memory). with linked list
    /// The transfer starts at buf0 and moves to the next buffer after a transfer, etc
    /// The last LLI entry has a link to the first buffer
    /// the buffer switching is in Hardware
    pub unsafe fn new_read_with_lli<W: Word, const M: usize, const N: usize>(
        channel: impl Peripheral<P = impl Channel> + 'a,
        request: Request,
        peri_addr: *mut W,
        llit: &mut LliTable<'a, W, M, N>,
        lli_option: LliOption,
        _options: TransferOptions,
    ) -> Self {
        into_ref!(channel);
        let channel: PeripheralRef<'a, AnyChannel> = channel.map_into();
        let data_size = W::size();
        let info = channel.info();
        let ch = info.dma.ch(info.num);

        // "Preceding reads and writes cannot be moved past subsequent writes."
        fence(Ordering::SeqCst);

        let this = Self { channel };

        #[cfg(dmamux)]
        super::dmamux::configure_dmamux(&*this.channel, request);

        ch.cr().write(|w| w.set_reset(true));
        ch.fcr().write(|w| w.0 = 0xFFFF_FFFF); // clear all irqs
        ch.tr1().write(|w| {
            w.set_sdw(data_size.into());
            w.set_ddw(data_size.into());
            w.set_sinc(false);
            w.set_dinc(true);
        });
        ch.tr2().write(|w| {
            w.set_dreq(vals::ChTr2Dreq::SOURCEPERIPHERAL);
            w.set_reqsel(request);
        });

        ch.sar().write_value(peri_addr as _); // Peripheral Addr
        llit.fixing_in_mem(lli_option);
        let llis_base_addr = ptr::addr_of!(llit.items[0]) as u32;
        ch.lbar().write(|reg| reg.set_lba((llis_base_addr >> 16) as u16)); // linked high addr
        ch.br1().write(|reg| reg.set_bndt((N * W::size().bytes()) as u16));
        ch.dar().write(|reg| *reg = llit.items[0].dar);
        ch.llr().write(|reg| reg.0 = llit.items[0].llr); // Set Start llr

        ch.cr().write(|w| {
            // Enable interrupts
            w.set_tcie(true);
            w.set_useie(true);
            w.set_dteie(true);
            w.set_suspie(true);

            // Start it
            w.set_en(true);
        });

        this
    }

    /// get the address value from the linked address register
    pub fn get_dar_reg(&self) -> u32 {
        let info = self.channel.info();
        let ch = info.dma.ch(info.num);
        ch.dar().read()
    }

    /// Request the transfer to stop.
    ///
    /// This doesn't immediately stop the transfer, you have to wait until [`is_running`](Self::is_running) returns false.
    pub fn request_stop(&mut self) {
        let info = self.channel.info();
        let ch = info.dma.ch(info.num);

        ch.cr().modify(|w| w.set_susp(true))
    }

    /// Return whether this transfer is still running.
    ///
    /// If this returns `false`, it can be because either the transfer finished, or
    /// it was requested to stop early with [`request_stop`](Self::request_stop).
    pub fn is_running(&mut self) -> bool {
        let info = self.channel.info();
        let ch = info.dma.ch(info.num);

        let sr = ch.sr().read();
        !sr.tcf() && !sr.suspf()
    }

    /// Gets the total remaining transfers for the channel
    /// Note: this will be zero for transfers that completed without cancellation.
    pub fn get_remaining_transfers(&self) -> u16 {
        let info = self.channel.info();
        let ch = info.dma.ch(info.num);

        ch.br1().read().bndt()
    }

    /// Blocking wait until the transfer finishes.
    pub fn blocking_wait(mut self) {
        while self.is_running() {}

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        fence(Ordering::SeqCst);

        core::mem::forget(self);
    }
}

impl<'a> Drop for Transfer<'a> {
    fn drop(&mut self) {
        self.request_stop();
        while self.is_running() {}

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        fence(Ordering::SeqCst);
    }
}

impl<'a> Unpin for Transfer<'a> {}
impl<'a> Future for Transfer<'a> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = &STATE[self.channel.id as usize];
        state.waker.register(cx.waker());

        if self.is_running() {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use super::bits::*;
use super::probes;
use super::queue::VirtQueues;
use super::{VirtioDevice, VirtioIntr, VqChange, VqIntr};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::ids::pci::VENDOR_VIRTIO;
use crate::hw::pci;
use crate::intr_pins::IntrPin;
use crate::migrate::MigrateStateError;
use crate::util::regmap::RegMap;

use lazy_static::lazy_static;

const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;

const VIRTIO_PCI_ISR_QUEUE: u8 = 1 << 0;
const VIRTIO_PCI_ISR_CFG: u8 = 1 << 1;

bitflags! {
    #[derive(Default)]
    pub struct Status: u8 {
        const RESET = 0;
        const ACK = 1;
        const DRIVER = 2;
        const DRIVER_OK = 4;
        const FEATURES_OK = 8;
        const NEEDS_RESET = 64;
        const FAILED = 128;
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum IntrMode {
    IsrOnly,
    IsrLintr,
    Msi,
}
impl From<pci::IntrMode> for IntrMode {
    fn from(pci_mode: pci::IntrMode) -> Self {
        match pci_mode {
            pci::IntrMode::Disabled => IntrMode::IsrOnly,
            pci::IntrMode::INTxPin => IntrMode::IsrLintr,
            pci::IntrMode::Msix => IntrMode::Msi,
        }
    }
}

struct VirtioState {
    status: Status,
    queue_sel: u16,
    nego_feat: u32,
    intr_mode: IntrMode,
    intr_mode_updating: bool,
    msix_cfg_vec: u16,
    msix_queue_vec: Vec<u16>,
}
impl VirtioState {
    fn new(num_queues: u16) -> Self {
        let mut msix_queue_vec = Vec::with_capacity(num_queues as usize);
        msix_queue_vec
            .resize_with(num_queues as usize, || VIRTIO_MSI_NO_VECTOR);
        Self {
            status: Status::RESET,
            queue_sel: 0,
            nego_feat: 0,
            intr_mode: IntrMode::IsrOnly,
            intr_mode_updating: false,
            msix_cfg_vec: VIRTIO_MSI_NO_VECTOR,
            msix_queue_vec,
        }
    }
    fn reset(&mut self) {
        self.status = Status::RESET;
        self.queue_sel = 0;
        self.nego_feat = 0;
        self.msix_cfg_vec = VIRTIO_MSI_NO_VECTOR;
    }
}

pub trait PciVirtio: VirtioDevice + Send + Sync + 'static {
    fn virtio_state(&self) -> &PciVirtioState;
    fn pci_state(&self) -> &pci::DeviceState;

    fn export(&self) -> migrate::PciVirtioStateV1 {
        let vs = self.virtio_state();
        let ps = self.pci_state();

        let queues = vs.queues.iter().map(|q| q.export()).collect();
        migrate::PciVirtioStateV1 {
            pci: ps.export(),
            state: vs.export(),
            queues,
        }
    }

    fn import(
        &self,
        input: migrate::PciVirtioStateV1,
    ) -> Result<(), MigrateStateError> {
        let vs = self.virtio_state();
        let ps = self.pci_state();

        // Keep Virtio in ISR-only mode (no lintr-pin or MSIs) during import so
        // it does not spuriously emit any observable interrupt events until all
        // of the state can be made consistent prior to any re-enabling.
        vs.set_intr_mode(ps, IntrMode::IsrOnly);

        // Bulk of the virtio state
        vs.import(input.state)?;

        // VirtQueue state
        for (vq, vq_input) in vs.queues.iter().zip(input.queues.into_iter()) {
            vq.import(vq_input)?;
        }

        // PCI state
        ps.import(input.pci)?;

        // Reconcile interrupt mode now that PCI state is populated too
        vs.set_intr_mode(ps, ps.get_intr_mode().into());

        Ok(())
    }
}

impl<D: PciVirtio + Send + Sync + 'static> pci::Device for D {
    fn device_state(&self) -> &pci::DeviceState {
        self.pci_state()
    }
    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp, ctx: &DispCtx) {
        let vs = self.virtio_state();

        assert_eq!(bar, pci::BarN::BAR0);
        let map = match vs.map_which.load(Ordering::SeqCst) {
            false => &vs.map_nomsix,
            true => &vs.map,
        };
        map.process(&mut rwo, |id, mut rwo| match id {
            VirtioTop::LegacyConfig => {
                LEGACY_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.legacy_read(self, id, ro, ctx),
                    RWOp::Write(wo) => {
                        vs.legacy_write(self.pci_state(), self, id, wo, ctx)
                    }
                })
            }
            VirtioTop::DeviceConfig => self.cfg_rw(rwo),
        });
    }
    fn attach(&self) {
        let ps = self.pci_state();
        if let Some(pin) = ps.lintr_pin() {
            let vs = self.virtio_state();
            vs.isr_state.set_pin(pin);
        }
    }
    fn interrupt_mode_change(&self, mode: pci::IntrMode) {
        let vs = self.virtio_state();
        vs.set_intr_mode(self.pci_state(), mode.into());

        // Make sure the correct legacy register map is used
        vs.map_which.store(mode == pci::IntrMode::Msix, Ordering::SeqCst);
    }
    fn msi_update(&self, info: pci::MsiUpdate, ctx: &DispCtx) {
        let vs = self.virtio_state();
        let mut state = vs.state.lock().unwrap();
        if state.intr_mode != IntrMode::Msi {
            return;
        }
        state =
            vs.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();
        state.intr_mode_updating = true;

        for vq in vs.queues.iter() {
            let val = *state.msix_queue_vec.get(vq.id as usize).unwrap();

            // avoid deadlock while modify per-VQ interrupt config
            drop(state);

            match info {
                pci::MsiUpdate::MaskAll | pci::MsiUpdate::UnmaskAll
                    if val != VIRTIO_MSI_NO_VECTOR =>
                {
                    self.queue_change(vq, VqChange::IntrCfg, ctx);
                }
                pci::MsiUpdate::Modify(idx) if val == idx => {
                    self.queue_change(vq, VqChange::IntrCfg, ctx);
                }
                _ => {}
            }

            state = vs.state.lock().unwrap();
        }
        state.intr_mode_updating = false;
        vs.state_cv.notify_all();
    }
}

pub struct PciVirtioState {
    pub queues: VirtQueues,

    state: Mutex<VirtioState>,
    state_cv: Condvar,
    isr_state: Arc<IsrState>,

    /// Quick access to register map for MSIX (true) or non-MSIX (false)
    map_which: AtomicBool,

    map: RegMap<VirtioTop>,
    map_nomsix: RegMap<VirtioTop>,
}
impl PciVirtioState {
    pub(super) fn create(
        queues: VirtQueues,
        msix_count: Option<u16>,
        dev_id: u16,
        sub_dev_id: u16,
        dev_class: u8,
        cfg_sz: usize,
    ) -> (Self, pci::DeviceState) {
        let mut builder = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_VIRTIO,
            device_id: dev_id,
            sub_vendor_id: VENDOR_VIRTIO,
            sub_device_id: sub_dev_id,
            class: dev_class,
            ..Default::default()
        })
        .add_lintr();

        if let Some(count) = msix_count {
            builder = builder.add_cap_msix(pci::BarN::BAR1, count);
        }

        // XXX: properly size the legacy cfg BAR
        builder = builder.add_bar_io(pci::BarN::BAR0, 0x200);
        let pci_state = builder.finish();

        let layout = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];
        let layout_nomsix = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ_NO_MSIX),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];

        let queue_count = queues.count().get();
        let this = Self {
            queues,

            state: Mutex::new(VirtioState::new(queue_count)),
            state_cv: Condvar::new(),
            isr_state: IsrState::new(),

            map: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ,
                &layout,
            ),
            map_nomsix: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ_NO_MSIX,
                &layout_nomsix,
            ),
            map_which: AtomicBool::new(false),
        };

        for queue in this.queues.iter() {
            queue.set_intr(IsrIntr::new(&this.isr_state));
        }

        (this, pci_state)
    }

    fn legacy_read(
        &self,
        dev: &dyn VirtioDevice,
        id: &LegacyReg,
        ro: &mut ReadOp,
        _ctx: &DispCtx,
    ) {
        match id {
            LegacyReg::FeatDevice => {
                ro.write_u32(self.features_supported(dev));
            }
            LegacyReg::FeatDriver => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.nego_feat);
            }
            LegacyReg::QueuePfn => {
                let state = self.state.lock().unwrap();
                if let Some(queue) = self.queues.get(state.queue_sel) {
                    let state = queue.get_state();
                    let addr = state.mapping.desc_addr;
                    ro.write_u32((addr >> PAGE_SHIFT) as u32);
                } else {
                    // bogus queue
                    ro.write_u32(0);
                }
            }
            LegacyReg::QueueSize => {
                ro.write_u16(self.queues.queue_size().get());
            }
            LegacyReg::QueueSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.queue_sel);
            }
            LegacyReg::QueueNotify => {}
            LegacyReg::DeviceStatus => {
                let state = self.state.lock().unwrap();
                ro.write_u8(state.status.bits());
            }
            LegacyReg::IsrStatus => {
                // reading ISR Status clears it as well
                let isr = self.isr_state.read_clear();
                ro.write_u8(isr);
            }
            LegacyReg::MsixVectorConfig => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.msix_cfg_vec);
            }
            LegacyReg::MsixVectorQueue => {
                let state = self.state.lock().unwrap();
                let val = state
                    .msix_queue_vec
                    .get(state.queue_sel as usize)
                    .unwrap_or(&VIRTIO_MSI_NO_VECTOR);
                ro.write_u16(*val);
            }
        }
    }
    fn legacy_write(
        &self,
        pci_state: &pci::DeviceState,
        dev: &dyn VirtioDevice,
        id: &LegacyReg,
        wo: &mut WriteOp,
        ctx: &DispCtx,
    ) {
        match id {
            LegacyReg::FeatDriver => {
                let nego = wo.read_u32() & self.features_supported(dev);
                let mut state = self.state.lock().unwrap();
                state.nego_feat = nego;
                dev.set_features(nego);
            }
            LegacyReg::QueuePfn => {
                let mut state = self.state.lock().unwrap();
                let mut success = false;
                let pfn = wo.read_u32();
                if let Some(queue) = self.queues.get(state.queue_sel) {
                    success = queue.map_legacy((pfn as u64) << PAGE_SHIFT);
                    dev.queue_change(queue, VqChange::Address, ctx);
                }
                if !success {
                    // XXX: interrupt needed?
                    state.status |= Status::FAILED;
                }
            }
            LegacyReg::QueueSelect => {
                let mut state = self.state.lock().unwrap();
                state.queue_sel = wo.read_u16();
            }
            LegacyReg::QueueNotify => {
                self.queue_notify(dev, wo.read_u16(), ctx);
            }
            LegacyReg::DeviceStatus => {
                self.set_status(dev, wo.read_u8(), ctx);
            }
            LegacyReg::MsixVectorConfig => {
                let mut state = self.state.lock().unwrap();
                state.msix_cfg_vec = wo.read_u16();
            }
            LegacyReg::MsixVectorQueue => {
                let hdl = pci_state.msix_hdl().unwrap();
                let mut state = self.state.lock().unwrap();
                let sel = state.queue_sel as usize;
                if let Some(queue) = self.queues.get(state.queue_sel) {
                    let val = wo.read_u16();

                    if state.intr_mode != IntrMode::Msi {
                        // Store the vector information for later
                        state.msix_queue_vec[sel] = val;
                    } else {
                        state = self
                            .state_cv
                            .wait_while(state, |s| s.intr_mode_updating)
                            .unwrap();
                        state.intr_mode_updating = true;
                        state.msix_queue_vec[sel] = val;

                        // State lock cannot be held while updating queue
                        // interrupt handlers due to deadlock possibility.
                        drop(state);
                        queue.set_intr(MsiIntr::new(hdl, val));
                        state = self.state.lock().unwrap();

                        // With the MSI configuration updated for the virtqueue,
                        // notify the device of the change
                        dev.queue_change(queue, VqChange::IntrCfg, ctx);

                        state.intr_mode_updating = false;
                        self.state_cv.notify_all();
                    }
                }
            }

            LegacyReg::FeatDevice
            | LegacyReg::QueueSize
            | LegacyReg::IsrStatus => {
                // Read-only regs
            }
        }
    }

    fn features_supported(&self, dev: &dyn VirtioDevice) -> u32 {
        dev.get_features() | VIRTIO_F_RING_INDIRECT_DESC as u32
    }
    fn set_status(&self, dev: &dyn VirtioDevice, status: u8, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        let val = Status::from_bits_truncate(status);
        if val == Status::RESET && state.status != Status::RESET {
            self.virtio_reset(dev, state, ctx)
        } else {
            // XXX: better device status FSM
            state.status = val;
        }
    }
    fn queue_notify(&self, dev: &dyn VirtioDevice, queue: u16, ctx: &DispCtx) {
        probes::virtio_vq_notify!(|| (
            dev as *const dyn VirtioDevice as *const c_void as u64,
            queue
        ));
        if let Some(vq) = self.queues.get(queue) {
            vq.live.store(true, Ordering::Release);
            dev.queue_notify(vq, ctx);
        }
    }

    /// Reset the virtio portion of the device
    ///
    /// This leaves PCI state (such as configured BARs) unchanged
    fn virtio_reset(
        &self,
        dev: &dyn VirtioDevice,
        mut state: MutexGuard<VirtioState>,
        ctx: &DispCtx,
    ) {
        for queue in self.queues.iter() {
            queue.reset();
            dev.queue_change(queue, VqChange::Reset, ctx);
        }
        state.reset();
        let _ = self.isr_state.read_clear();
        VirtioDevice::reset(dev, ctx);
    }

    pub fn reset<D>(&self, dev: &D, ctx: &DispCtx)
    where
        D: pci::Device + PciVirtio,
    {
        let vs = dev.virtio_state();
        let ps = dev.pci_state();

        let state = vs.state.lock().unwrap();
        vs.virtio_reset(dev, state, ctx);
        ps.reset(dev);
    }

    fn set_intr_mode(&self, pci_state: &pci::DeviceState, new_mode: IntrMode) {
        let mut state = self.state.lock().unwrap();
        let old_mode = state.intr_mode;
        if new_mode == old_mode {
            return;
        }

        state =
            self.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();

        state.intr_mode_updating = true;
        match old_mode {
            IntrMode::IsrLintr => {
                // When leaving lintr-pin mode, deassert anything on said pin
                self.isr_state.disable();
            }
            IntrMode::Msi => {
                // When leaving MSI mode, re-wire the Isr interrupt handling
                //
                // To avoid deadlock, the state lock must be dropped while
                // updating the interrupts handlers on queues.
                drop(state);
                for queue in self.queues.iter() {
                    queue.set_intr(IsrIntr::new(&self.isr_state));
                }
                state = self.state.lock().unwrap();
            }
            _ => {}
        }

        state.intr_mode = new_mode;
        match new_mode {
            IntrMode::IsrLintr => {
                self.isr_state.enable();
            }
            IntrMode::Msi => {
                let hdl = pci_state.msix_hdl().unwrap();
                for vq in self.queues.iter() {
                    let vec =
                        *state.msix_queue_vec.get(vq.id as usize).unwrap();

                    // State lock cannot be held while updating queue interrupt
                    // handlers due to deadlock possibility.
                    drop(state);
                    vq.set_intr(MsiIntr::new(hdl.clone(), vec));
                    state = self.state.lock().unwrap();
                }
            }
            _ => {}
        }
        state.intr_mode_updating = false;
        self.state_cv.notify_all();
    }

    pub fn negotiated_features(&self) -> u32 {
        let state = self.state.lock().unwrap();
        state.nego_feat
    }

    pub fn export(&self) -> migrate::DeviceStateV1 {
        let state = self.state.lock().unwrap();
        let (isr_queue, isr_cfg) = self.isr_state.read();

        migrate::DeviceStateV1 {
            status: state.status.bits(),
            queue_sel: state.queue_sel,
            nego_feat: state.nego_feat,
            msix_cfg_vec: state.msix_cfg_vec,
            msix_queue_vec: state.msix_queue_vec.clone(),
            isr_queue,
            isr_cfg,
        }
    }

    pub fn import(
        &self,
        input: migrate::DeviceStateV1,
    ) -> Result<(), MigrateStateError> {
        let mut state = self.state.lock().unwrap();
        state.status = Status::from_bits(input.status).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "virtio status: failed to import saved value {:#x}",
                state.status
            ))
        })?;
        state.queue_sel = input.queue_sel;
        state.nego_feat = input.nego_feat;
        state.msix_cfg_vec = input.msix_cfg_vec;
        state.msix_queue_vec = input.msix_queue_vec;
        self.isr_state.write(input.isr_queue, input.isr_cfg);

        Ok(())
    }
}

#[derive(Default)]
struct IsrInner {
    disabled: bool,
    intr_queue: bool,
    intr_cfg: bool,
    pin: Option<Arc<dyn IntrPin>>,
}
impl IsrInner {
    fn raised(&self) -> bool {
        self.intr_queue || self.intr_cfg
    }
}
struct IsrState {
    inner: Mutex<IsrInner>,
}
impl IsrState {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(IsrInner::default()) })
    }
    /// Raise queue ISR condition
    fn raise_queue(&self) {
        self.sync_pin(|inner| {
            inner.intr_queue = true;
        });
    }
    /// Read ISR value, then clear it.
    fn read_clear(&self) -> u8 {
        let (mut queue, mut cfg) = (false, false);
        self.sync_pin(|inner| {
            queue = inner.intr_queue;
            cfg = inner.intr_cfg;
            inner.intr_queue = false;
            inner.intr_cfg = false;
        });
        let mut val = 0;
        if queue {
            val |= VIRTIO_PCI_ISR_QUEUE;
        }
        if cfg {
            val |= VIRTIO_PCI_ISR_CFG;
        }
        val
    }
    /// Read ISR value.  Returns (`intr_queue`, `intr_cfg`)
    fn read(&self) -> (bool, bool) {
        let inner = self.inner.lock().unwrap();
        (inner.intr_queue, inner.intr_cfg)
    }
    /// Write ISR value
    fn write(&self, intr_queue: bool, intr_cfg: bool) {
        self.sync_pin(|inner| {
            inner.intr_queue = intr_queue;
            inner.intr_cfg = intr_cfg;
        });
    }
    /// Sync ISR state with any associated interrupt pin
    fn sync_pin(&self, f: impl FnOnce(&mut IsrInner)) {
        let mut inner = self.inner.lock().unwrap();

        let raised_before = inner.raised();
        f(&mut inner);
        let raised_after = inner.raised();

        // Sync pin state with ISR value
        if !inner.disabled {
            if !raised_before && raised_after {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.assert()
                }
            }
            if raised_before && !raised_after {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.deassert()
                }
            }
        }
    }
    /// Disable state emission via interrupt pin
    fn disable(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.disabled {
            if let Some(pin) = inner.pin.as_ref() {
                pin.deassert();
            }
            inner.disabled = true;
        }
    }
    /// Enable state emission via interrupt pin
    fn enable(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.disabled {
            if inner.intr_queue || inner.intr_cfg {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.assert();
                }
            }
            inner.disabled = false;
        }
    }
    /// Set underlying interrupt pin. (Must be performed only once)
    fn set_pin(&self, pin: Arc<dyn IntrPin>) {
        let mut inner = self.inner.lock().unwrap();
        let old = inner.pin.replace(pin);
        // XXX: strict for now
        assert!(old.is_none());
    }
}

struct IsrIntr {
    state: Weak<IsrState>,
}
impl IsrIntr {
    fn new(state: &Arc<IsrState>) -> Box<Self> {
        Box::new(Self { state: Arc::downgrade(state) })
    }
}
impl VirtioIntr for IsrIntr {
    fn notify(&self, _ctx: &DispCtx) {
        if let Some(state) = Weak::upgrade(&self.state) {
            state.raise_queue()
        }
    }
    fn read(&self) -> VqIntr {
        VqIntr::Pin
    }
}

struct MsiIntr {
    hdl: pci::MsixHdl,
    index: u16,
}
impl MsiIntr {
    fn new(hdl: pci::MsixHdl, index: u16) -> Box<Self> {
        Box::new(Self { hdl, index })
    }
}
impl VirtioIntr for MsiIntr {
    fn notify(&self, ctx: &DispCtx) {
        if self.index < self.hdl.count() {
            self.hdl.fire(self.index, ctx);
        }
    }
    fn read(&self) -> VqIntr {
        if self.index < self.hdl.count() {
            let data = self.hdl.read(self.index);
            VqIntr::Msi(data.addr, data.data, data.masked)
        } else {
            VqIntr::Pin
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum VirtioTop {
    LegacyConfig,
    DeviceConfig,
}

const LEGACY_REG_SZ: usize = 0x18;
const LEGACY_REG_SZ_NO_MSIX: usize = 0x14;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum LegacyReg {
    FeatDevice,
    FeatDriver,
    QueuePfn,
    QueueSize,
    QueueSelect,
    QueueNotify,
    DeviceStatus,
    IsrStatus,
    MsixVectorConfig,
    MsixVectorQueue,
}
lazy_static! {
    static ref LEGACY_REGS: RegMap<LegacyReg> = {
        let layout = [
            (LegacyReg::FeatDevice, 4),
            (LegacyReg::FeatDriver, 4),
            (LegacyReg::QueuePfn, 4),
            (LegacyReg::QueueSize, 2),
            (LegacyReg::QueueSelect, 2),
            (LegacyReg::QueueNotify, 2),
            (LegacyReg::DeviceStatus, 1),
            (LegacyReg::IsrStatus, 1),
            (LegacyReg::MsixVectorConfig, 2),
            (LegacyReg::MsixVectorQueue, 2),
        ];
        RegMap::create_packed(LEGACY_REG_SZ, &layout, None)
    };
}

pub mod migrate {
    use crate::hw::pci::migrate::PciStateV1;
    use crate::hw::virtio::queue;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub enum IntrModeV1 {
        IsrOnly,
        IsrLintr,
        Msi,
    }
    impl From<super::IntrMode> for IntrModeV1 {
        fn from(inp: super::IntrMode) -> Self {
            match inp {
                super::IntrMode::IsrOnly => IntrModeV1::IsrOnly,
                super::IntrMode::IsrLintr => IntrModeV1::IsrLintr,
                super::IntrMode::Msi => IntrModeV1::Msi,
            }
        }
    }

    #[derive(Deserialize, Serialize)]
    pub struct DeviceStateV1 {
        pub status: u8,
        pub queue_sel: u16,
        pub nego_feat: u32,
        pub msix_cfg_vec: u16,
        pub msix_queue_vec: Vec<u16>,
        pub isr_queue: bool,
        pub isr_cfg: bool,
    }

    #[derive(Deserialize, Serialize)]
    pub struct PciVirtioStateV1 {
        pub pci: PciStateV1,
        pub state: DeviceStateV1,
        pub queues: Vec<queue::migrate::VirtQueueV1>,
    }
}

use bitvec::prelude as bv;
use futures::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use propolis::common::GuestAddr;
use propolis::instance::MigrateRole;
use propolis::migrate::{MigrateStateError, Migrator};
use propolis_client::instance_spec::MigrationCompatible;
use slog::{error, info, warn};
use std::io;
use std::sync::Arc;
use tokio_util::codec::Framed;

use crate::migrate::codec::{self, LiveMigrationFramer};
use crate::migrate::memx;
use crate::migrate::preamble::Preamble;
use crate::migrate::{
    Device, MigrateContext, MigrateError, MigrationState, PageIter,
};

pub async fn migrate(
    mctx: Arc<MigrateContext>,
    conn: Upgraded,
) -> Result<(), MigrateError> {
    let mut proto = DestinationProtocol::new(mctx, conn);

    if let Err(err) = proto.run().await {
        proto.mctx.set_state(MigrationState::Error).await;
        // We encountered an error, try to inform the remote before bailing
        // Note, we don't use `?` here as this is a best effort and we don't
        // want an error encountered during this send to shadow the run error
        // from the caller.
        let _ = proto.conn.send(codec::Message::Error(err.clone())).await;
        return Err(err);
    }

    Ok(())
}

struct DestinationProtocol {
    /// The migration context which also contains the `Instance` handle.
    mctx: Arc<MigrateContext>,

    /// Transport to the source Instance.
    conn: Framed<Upgraded, LiveMigrationFramer>,
}

impl DestinationProtocol {
    fn new(mctx: Arc<MigrateContext>, conn: Upgraded) -> Self {
        let codec_log = mctx.log.new(slog::o!());
        Self {
            mctx,
            conn: Framed::new(conn, LiveMigrationFramer::new(codec_log)),
        }
    }

    fn log(&self) -> &slog::Logger {
        &self.mctx.log
    }

    async fn run(&mut self) -> Result<(), MigrateError> {
        self.start();
        self.sync().await?;
        self.ram_push().await?;
        self.device_state().await?;
        self.arch_state().await?;
        self.ram_pull().await?;
        self.finish().await?;
        self.end();
        Ok(())
    }

    fn start(&mut self) {
        info!(self.log(), "Entering Destination Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Sync).await;
        let preamble: Preamble = match self.read_msg().await? {
            codec::Message::Serialized(s) => {
                Ok(ron::de::from_str(&s).map_err(codec::ProtocolError::from)?)
            }
            msg => {
                error!(
                    self.log(),
                    "expected serialized preamble but received: {msg:?}"
                );
                Err(MigrateError::UnexpectedMessage)
            }
        }?;
        info!(self.log(), "Destination read Preamble: {:?}", preamble);
        if !preamble
            .instance_spec
            .is_migration_compatible(&self.mctx.instance_spec)
        {
            error!(
                self.log(),
                "Source and destination instance specs incompatible"
            );
            return Err(MigrateError::InvalidInstanceState);
        }
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPush).await;
        let (dirty, highest) = self.query_ram().await?;
        for (k, region) in dirty.as_raw_slice().chunks(4096).enumerate() {
            if region.iter().all(|&b| b == 0) {
                continue;
            }
            let start = (k * 4096 * 8 * 4096) as u64;
            let end = start + (region.len() * 8 * 4096) as u64;
            let end = highest.min(end);
            self.send_msg(memx::make_mem_fetch(start, end, region)).await?;
            let m = self.read_msg().await?;
            info!(self.log(), "ram_push: source xfer phase recvd {:?}", m);
            match m {
                codec::Message::MemXfer(start, end, bits) => {
                    if !memx::validate_bitmap(start, end, &bits) {
                        error!(
                            self.log(),
                            "ram_push: MemXfer received bad bitmap"
                        );
                        return Err(MigrateError::Phase);
                    }
                    // XXX: We should do stricter validation on the fetch
                    // request here.  For instance, we shouldn't "push" MMIO
                    // space or non-existent RAM regions.  While we de facto
                    // do not because of the way access is implemented, we
                    // should probably disallow it at the protocol level.
                    self.xfer_ram(start, end, &bits).await?;
                }
                _ => return Err(MigrateError::UnexpectedMessage),
            };
        }
        self.send_msg(codec::Message::MemDone).await?;
        self.mctx.set_state(MigrationState::Pause).await;
        Ok(())
    }

    async fn query_ram(
        &mut self,
    ) -> Result<(bv::BitVec<u8, bv::Lsb0>, u64), MigrateError> {
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;

        let mut dirty = bv::BitVec::<u8, bv::Lsb0>::new();
        let mut highest = 0;
        loop {
            let m = self.read_msg().await?;
            info!(self.log(), "ram_push: source xfer phase recvd {:?}", m);
            match m {
                codec::Message::MemEnd(start, end) => {
                    if start != 0 || end != !0 {
                        error!(self.log(), "ram_push: received bad MemEnd");
                        return Err(MigrateError::Phase);
                    }
                    break;
                }
                codec::Message::MemOffer(start, end, bits) => {
                    if !memx::validate_bitmap(start, end, &bits) {
                        error!(
                            self.log(),
                            "ram_push: MemOffer received bad bitmap"
                        );
                        return Err(MigrateError::Phase);
                    }
                    if end > highest {
                        highest = end;
                    }
                    let start_bit_index = start as usize / 4096;
                    if dirty.len() < start_bit_index {
                        dirty.resize(start_bit_index, false);
                    }
                    dirty.extend_from_raw_slice(&bits);
                }
                _ => return Err(MigrateError::UnexpectedMessage),
            }
        }
        Ok((dirty, highest))
    }

    async fn xfer_ram(
        &mut self,
        start: u64,
        end: u64,
        bits: &[u8],
    ) -> Result<(), MigrateError> {
        info!(self.log(), "ram_push: xfer RAM between {} and {}", start, end);
        for addr in PageIter::new(start, end, bits) {
            let bytes = self.read_page().await?;
            self.write_guest_ram(GuestAddr(addr), &bytes).await?;
        }
        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Device).await;

        let devices: Vec<Device> = match self.read_msg().await? {
            codec::Message::Serialized(encoded) => {
                ron::de::from_reader(encoded.as_bytes())
                    .map_err(codec::ProtocolError::from)?
            }
            msg => {
                error!(self.log(), "device_state: unexpected message: {msg:?}");
                return Err(MigrateError::UnexpectedMessage);
            }
        };
        self.read_ok().await?;

        let dispctx = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or_else(|| MigrateError::InstanceNotInitialized)?;

        info!(self.log(), "Devices: {devices:#?}");

        let inv = self.mctx.instance.inv();
        for device in devices {
            info!(
                self.log(),
                "Applying state to device {}", device.instance_name
            );

            let dev_ent =
                inv.get_by_name(&device.instance_name).ok_or_else(|| {
                    MigrateError::UnknownDevice(device.instance_name.clone())
                })?;

            match dev_ent.migrate() {
                Migrator::NonMigratable => {
                    error!(self.log(), "Can't migrate instance with non-migratable device ({})", device.instance_name);
                    return Err(MigrateError::DeviceState(
                        MigrateStateError::NonMigratable,
                    ));
                }
                Migrator::Simple => {
                    // The source shouldn't be sending devices with empty payloads
                    warn!(
                        self.log(),
                        "received unexpected device state for device {}",
                        device.instance_name
                    );
                }
                Migrator::Custom(migrate) => {
                    let mut deserializer =
                        ron::Deserializer::from_str(&device.payload)
                            .map_err(codec::ProtocolError::from)?;
                    let deserializer =
                        &mut <dyn erased_serde::Deserializer>::erase(
                            &mut deserializer,
                        );
                    migrate.import(
                        dev_ent.type_name(),
                        deserializer,
                        &dispctx,
                    )?;
                }
            }
        }
        drop(dispctx);

        self.send_msg(codec::Message::Okay).await
    }

    async fn arch_state(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Arch).await;
        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPull).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Finish).await;

        // Allow the destination to be resumed now that the migration is
        // complete.
        self.mctx
            .instance
            .set_target_state(propolis::instance::ReqState::MigrateResume)?;
        self.send_msg(codec::Message::Okay).await?;
        let _ = self.read_ok().await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) {
        info!(self.log(), "Destination Migration Successful");
    }

    async fn read_msg(&mut self) -> Result<codec::Message, MigrateError> {
        self.conn
            .next()
            .await
            .ok_or_else(|| {
                codec::ProtocolError::Io(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                ))
            })?
            // If this is an error message, lift that out
            .map(|msg| match msg {
                codec::Message::Error(err) => {
                    error!(self.log(), "remote error: {err}");
                    Err(MigrateError::RemoteError(
                        MigrateRole::Source,
                        err.to_string(),
                    ))
                }
                msg => Ok(msg),
            })?
    }

    async fn read_ok(&mut self) -> Result<(), MigrateError> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            msg => {
                error!(self.log(), "expected `Okay` but received: {msg:?}");
                Err(MigrateError::UnexpectedMessage)
            }
        }
    }

    async fn read_page(&mut self) -> Result<Vec<u8>, MigrateError> {
        match self.read_msg().await? {
            codec::Message::Page(bytes) => Ok(bytes),
            _ => Err(MigrateError::UnexpectedMessage),
        }
    }

    async fn send_msg(
        &mut self,
        m: codec::Message,
    ) -> Result<(), MigrateError> {
        Ok(self.conn.send(m).await?)
    }

    async fn write_guest_ram(
        &mut self,
        addr: GuestAddr,
        buf: &[u8],
    ) -> Result<(), MigrateError> {
        let memctx = self
            .mctx
            .async_ctx
            .dispctx()
            .await
            .ok_or(MigrateError::InstanceNotInitialized)?
            .mctx
            .memctx();
        let len = buf.len();
        memctx.write_from(addr, buf, len);
        Ok(())
    }
}

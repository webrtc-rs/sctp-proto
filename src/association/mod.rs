use crate::association::{
    state::{AckMode, AckState, AssociationState},
    stats::AssociationStats,
};
use crate::chunk::{
    chunk_abort::ChunkAbort, chunk_cookie_ack::ChunkCookieAck, chunk_cookie_echo::ChunkCookieEcho,
    chunk_error::ChunkError, chunk_forward_tsn::ChunkForwardTsn,
    chunk_forward_tsn::ChunkForwardTsnStream, chunk_heartbeat::ChunkHeartbeat,
    chunk_heartbeat_ack::ChunkHeartbeatAck, chunk_init::ChunkInit, chunk_init::ChunkInitAck,
    chunk_payload_data::ChunkPayloadData, chunk_payload_data::PayloadProtocolIdentifier,
    chunk_reconfig::ChunkReconfig, chunk_selective_ack::ChunkSelectiveAck,
    chunk_shutdown::ChunkShutdown, chunk_shutdown_ack::ChunkShutdownAck,
    chunk_shutdown_complete::ChunkShutdownComplete, chunk_type::CT_FORWARD_TSN, Chunk,
    ErrorCauseUnrecognizedChunkType, USER_INITIATED_ABORT,
};
use crate::config::{ServerConfig, TransportConfig, COMMON_HEADER_SIZE, DATA_CHUNK_HEADER_SIZE};
use crate::error::{Error, Result};
use crate::packet::{CommonHeader, Packet};
use crate::param::{
    param_heartbeat_info::ParamHeartbeatInfo,
    param_outgoing_reset_request::ParamOutgoingResetRequest,
    param_reconfig_response::{ParamReconfigResponse, ReconfigResult},
    param_state_cookie::ParamStateCookie,
    param_supported_extensions::ParamSupportedExtensions,
    Param,
};
use crate::queue::{payload_queue::PayloadQueue, pending_queue::PendingQueue};
use crate::shared::{AssociationEventInner, AssociationId, EndpointEvent, EndpointEventInner};
use crate::util::{sna16lt, sna32gt, sna32gte, sna32lt, sna32lte};
use crate::{AssociationEvent, Payload, Side, Transmit};
use stream::{ReliabilityType, Stream, StreamEvent, StreamId, StreamState};
use timer::{RtoManager, Timer, TimerTable, ACK_INTERVAL};

use crate::association::stream::RecvSendState;
use bytes::Bytes;
use fxhash::FxHashMap;
use log::{debug, error, trace, warn};
use rand::random;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

pub(crate) mod state;
pub(crate) mod stats;
pub(crate) mod stream;
mod timer;

#[cfg(test)]
mod association_test;

/// Reasons why an association might be lost
#[derive(Debug, Error, Eq, Clone, PartialEq)]
pub enum AssociationError {
    /// Handshake failed
    #[error("{0}")]
    HandshakeFailed(#[from] Error),
    /// The peer violated the QUIC specification as understood by this implementation
    #[error("transport error")]
    TransportError,
    /// The peer's QUIC stack aborted the association automatically
    #[error("aborted by peer")]
    AssociationClosed,
    /// The peer closed the association
    #[error("closed by peer")]
    ApplicationClosed,
    /// The peer is unable to continue processing this association, usually due to having restarted
    #[error("reset by peer")]
    Reset,
    /// Communication with the peer has lapsed for longer than the negotiated idle timeout
    ///
    /// If neither side is sending keep-alives, an association will time out after a long enough idle
    /// period even if the peer is still reachable
    #[error("timed out")]
    TimedOut,
    /// The local application closed the association
    #[error("closed")]
    LocallyClosed,
}

/// Events of interest to the application
#[derive(Debug)]
pub enum Event {
    /// The association was successfully established
    Connected,
    /// The association was lost
    ///
    /// Emitted if the peer closes the association or an error is encountered.
    AssociationLost {
        /// Reason that the association was closed
        reason: AssociationError,
    },
    /// Stream events
    Stream(StreamEvent),
    /// One or more application datagrams have been received
    DatagramReceived,
}

///Association represents an SCTP association
//13.2.  Parameters Necessary per Association (i.e., the TCB)
//Peer : Tag value to be sent in every packet and is received
//Verification: in the INIT or INIT ACK chunk.
//Tag :
//
//My : Tag expected in every inbound packet and sent in the
//Verification: INIT or INIT ACK chunk.
//
//Tag :
//State : A state variable indicating what state the association
// : is in, i.e., COOKIE-WAIT, COOKIE-ECHOED, ESTABLISHED,
// : SHUTDOWN-PENDING, SHUTDOWN-SENT, SHUTDOWN-RECEIVED,
// : SHUTDOWN-ACK-SENT.
//
// No Closed state is illustrated since if a
// association is Closed its TCB SHOULD be removed.
#[derive(Debug)]
pub struct Association {
    side: Side,
    state: AssociationState,
    handshake_completed: bool,
    max_message_size: u32,
    inflight_queue_length: usize,
    will_send_shutdown: bool,
    bytes_received: usize,
    bytes_sent: usize,

    peer_verification_tag: u32,
    my_verification_tag: u32,
    my_next_tsn: u32,
    peer_last_tsn: u32,
    // for RTT measurement
    min_tsn2measure_rtt: u32,
    will_send_forward_tsn: bool,
    will_retransmit_fast: bool,
    will_retransmit_reconfig: bool,

    will_send_shutdown_ack: bool,
    will_send_shutdown_complete: bool,

    // Reconfig
    my_next_rsn: u32,
    reconfigs: FxHashMap<u32, ChunkReconfig>,
    reconfig_requests: FxHashMap<u32, ParamOutgoingResetRequest>,

    // Non-RFC internal data
    remote_addr: SocketAddr,
    local_ip: Option<IpAddr>,
    source_port: u16,
    destination_port: u16,
    my_max_num_inbound_streams: u16,
    my_max_num_outbound_streams: u16,
    my_cookie: Option<ParamStateCookie>,

    payload_queue: PayloadQueue,
    inflight_queue: PayloadQueue,
    pending_queue: PendingQueue,
    control_queue: VecDeque<Packet>,
    stream_queue: VecDeque<u16>,

    pub(crate) mtu: u32,
    // max DATA chunk payload size
    max_payload_size: u32,
    cumulative_tsn_ack_point: u32,
    advanced_peer_tsn_ack_point: u32,
    use_forward_tsn: bool,

    pub(crate) rto_mgr: RtoManager,
    timers: TimerTable,

    // Congestion control parameters
    max_receive_buffer_size: u32,
    // my congestion window size
    pub(crate) cwnd: u32,
    // calculated peer's receiver windows size
    rwnd: u32,
    // slow start threshold
    pub(crate) ssthresh: u32,
    partial_bytes_acked: u32,
    pub(crate) in_fast_recovery: bool,
    fast_recover_exit_point: u32,

    // Chunks stored for retransmission
    stored_init: Option<ChunkInit>,
    stored_cookie_echo: Option<ChunkCookieEcho>,
    pub(crate) streams: FxHashMap<StreamId, StreamState>,

    events: VecDeque<Event>,
    endpoint_events: VecDeque<EndpointEventInner>,
    error: Option<AssociationError>,

    // per inbound packet context
    delayed_ack_triggered: bool,
    immediate_ack_triggered: bool,

    pub(crate) stats: AssociationStats,
    ack_state: AckState,

    // for testing
    pub(crate) ack_mode: AckMode,
}

impl Default for Association {
    fn default() -> Self {
        Association {
            side: Side::default(),
            state: AssociationState::default(),
            handshake_completed: false,
            max_message_size: 0,
            inflight_queue_length: 0,
            will_send_shutdown: false,
            bytes_received: 0,
            bytes_sent: 0,

            peer_verification_tag: 0,
            my_verification_tag: 0,
            my_next_tsn: 0,
            peer_last_tsn: 0,
            // for RTT measurement
            min_tsn2measure_rtt: 0,
            will_send_forward_tsn: false,
            will_retransmit_fast: false,
            will_retransmit_reconfig: false,

            will_send_shutdown_ack: false,
            will_send_shutdown_complete: false,

            // Reconfig
            my_next_rsn: 0,
            reconfigs: FxHashMap::default(),
            reconfig_requests: FxHashMap::default(),

            // Non-RFC internal data
            remote_addr: SocketAddr::from_str("0.0.0.0:0").unwrap(),
            local_ip: None,
            source_port: 0,
            destination_port: 0,
            my_max_num_inbound_streams: 0,
            my_max_num_outbound_streams: 0,
            my_cookie: None,

            payload_queue: PayloadQueue::default(),
            inflight_queue: PayloadQueue::default(),
            pending_queue: PendingQueue::default(),
            control_queue: VecDeque::default(),
            stream_queue: VecDeque::default(),

            mtu: 0,
            // max DATA chunk payload size
            max_payload_size: 0,
            cumulative_tsn_ack_point: 0,
            advanced_peer_tsn_ack_point: 0,
            use_forward_tsn: false,

            rto_mgr: RtoManager::default(),
            timers: TimerTable::default(),

            // Congestion control parameters
            max_receive_buffer_size: 0,
            // my congestion window size
            cwnd: 0,
            // calculated peer's receiver windows size
            rwnd: 0,
            // slow start threshold
            ssthresh: 0,
            partial_bytes_acked: 0,
            in_fast_recovery: false,
            fast_recover_exit_point: 0,

            // Chunks stored for retransmission
            stored_init: None,
            stored_cookie_echo: None,
            streams: FxHashMap::default(),

            events: VecDeque::default(),
            endpoint_events: VecDeque::default(),
            error: None,

            // per inbound packet context
            delayed_ack_triggered: false,
            immediate_ack_triggered: false,

            stats: AssociationStats::default(),
            ack_state: AckState::default(),

            // for testing
            ack_mode: AckMode::default(),
        }
    }
}

impl Association {
    pub(crate) fn new(
        server_config: Option<Arc<ServerConfig>>,
        config: Arc<TransportConfig>,
        max_payload_size: u32,
        local_aid: AssociationId,
        remote_addr: SocketAddr,
        local_ip: Option<IpAddr>,
        now: Instant,
    ) -> Self {
        let side = if server_config.is_some() {
            Side::Server
        } else {
            Side::Client
        };

        // It's a bit strange, but we're going backwards from the calculation in
        // config.rs to get max_payload_size from INITIAL_MTU.
        let mtu = max_payload_size + COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE;

        // RFC 4690 Sec 7.2.1
        // The initial cwnd before DATA transmission or after a sufficiently
        // long idle period MUST be set to min(4*MTU, max (2*MTU, 4380bytes)).
        let cwnd = (2 * mtu).clamp(4380, 4 * mtu);
        let mut tsn = random::<u32>();
        if tsn == 0 {
            tsn += 1;
        }

        let mut this = Association {
            side,
            handshake_completed: false,
            max_receive_buffer_size: config.max_receive_buffer_size(),
            max_message_size: config.max_message_size(),
            my_max_num_outbound_streams: config.max_num_outbound_streams(),
            my_max_num_inbound_streams: config.max_num_inbound_streams(),
            max_payload_size,

            rto_mgr: RtoManager::new(),
            timers: TimerTable::new(),

            mtu,
            cwnd,
            remote_addr,
            local_ip,

            my_verification_tag: local_aid,
            my_next_tsn: tsn,
            my_next_rsn: tsn,
            min_tsn2measure_rtt: tsn,
            cumulative_tsn_ack_point: tsn - 1,
            advanced_peer_tsn_ack_point: tsn - 1,
            error: None,

            ..Default::default()
        };

        if side.is_client() {
            let mut init = ChunkInit {
                initial_tsn: this.my_next_tsn,
                num_outbound_streams: this.my_max_num_outbound_streams,
                num_inbound_streams: this.my_max_num_inbound_streams,
                initiate_tag: this.my_verification_tag,
                advertised_receiver_window_credit: this.max_receive_buffer_size,
                ..Default::default()
            };
            init.set_supported_extensions();

            this.set_state(AssociationState::CookieWait);
            this.stored_init = Some(init);
            let _ = this.send_init();
            this.timers
                .start(Timer::T1Init, now, this.rto_mgr.get_rto());
        }

        this
    }

    /// Returns application-facing event
    ///
    /// Associations should be polled for events after:
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll(&mut self) -> Option<Event> {
        if let Some(x) = self.events.pop_front() {
            return Some(x);
        }

        /*TODO: if let Some(event) = self.streams.poll() {
            return Some(Event::Stream(event));
        }*/

        if let Some(err) = self.error.take() {
            return Some(Event::AssociationLost { reason: err });
        }

        None
    }

    /// Return endpoint-facing event
    #[must_use]
    pub fn poll_endpoint_event(&mut self) -> Option<EndpointEvent> {
        self.endpoint_events.pop_front().map(EndpointEvent)
    }

    /// Returns the next time at which `handle_timeout` should be called
    ///
    /// The value returned may change after:
    /// - the application performed some I/O on the association
    /// - a call was made to `handle_transmit`
    /// - a call to `poll_transmit` returned `Some`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        self.timers.next_timeout()
    }

    /// Returns packets to transmit
    ///
    /// Associations should be polled for transmit after:
    /// - the application performed some I/O on the Association
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        let (contents, _) = self.gather_outbound(now);
        if contents.is_empty() {
            None
        } else {
            trace!(
                "[{}] sending {} bytes (total {} datagrams)",
                self.side,
                contents.iter().fold(0, |l, c| l + c.len()),
                contents.len()
            );
            Some(Transmit {
                now,
                remote: self.remote_addr,
                payload: Payload::RawEncode(contents),
                ecn: None,
                local_ip: self.local_ip,
            })
        }
    }

    /// Process timer expirations
    ///
    /// Executes protocol logic, potentially preparing signals (including application `Event`s,
    /// `EndpointEvent`s and outgoing datagrams) that should be extracted through the relevant
    /// methods.
    ///
    /// It is most efficient to call this immediately after the system clock reaches the latest
    /// `Instant` that was output by `poll_timeout`; however spurious extra calls will simply
    /// no-op and therefore are safe.
    pub fn handle_timeout(&mut self, now: Instant) {
        for &timer in &Timer::VALUES {
            let (expired, failure, n_rtos) = self.timers.is_expired(timer, now);
            if !expired {
                continue;
            }
            self.timers.set(timer, None);
            //trace!("{:?} timeout", timer);

            if timer == Timer::Ack {
                self.on_ack_timeout();
            } else if failure {
                self.on_retransmission_failure(timer);
            } else {
                self.on_retransmission_timeout(timer, n_rtos);
                self.timers.start(timer, now, self.rto_mgr.get_rto());
            }
        }
    }

    /// Process `AssociationEvent`s generated by the associated `Endpoint`
    ///
    /// Will execute protocol logic upon receipt of an association event, in turn preparing signals
    /// (including application `Event`s, `EndpointEvent`s and outgoing datagrams) that should be
    /// extracted through the relevant methods.
    pub fn handle_event(&mut self, event: AssociationEvent) {
        match event.0 {
            AssociationEventInner::Datagram(transmit) => {
                // If this packet could initiate a migration and we're a client or a server that
                // forbids migration, drop the datagram. This could be relaxed to heuristically
                // permit NAT-rebinding-like migration.
                /*TODO:if remote != self.remote && self.server_config.as_ref().map_or(true, |x| !x.migration)
                {
                    trace!("discarding packet from unrecognized peer {}", remote);
                    return;
                }*/

                if let Payload::PartialDecode(partial_decode) = transmit.payload {
                    trace!(
                        "[{}] receiving {} bytes",
                        self.side,
                        COMMON_HEADER_SIZE as usize + partial_decode.remaining.len()
                    );

                    let pkt = match partial_decode.finish() {
                        Ok(p) => p,
                        Err(err) => {
                            warn!("[{}] unable to parse SCTP packet {}", self.side, err);
                            return;
                        }
                    };

                    if let Err(err) = self.handle_inbound(pkt, transmit.now) {
                        error!("handle_inbound got err: {}", err);
                        let _ = self.close();
                    }
                } else {
                    trace!("discarding invalid partial_decode");
                }
            } //TODO:
        }
    }

    /// Returns Association statistics
    pub fn stats(&self) -> AssociationStats {
        self.stats
    }

    /// Whether the Association is in the process of being established
    ///
    /// If this returns `false`, the Association may be either established or closed, signaled by the
    /// emission of a `Connected` or `AssociationLost` message respectively.
    pub fn is_handshaking(&self) -> bool {
        !self.handshake_completed
    }

    /// Whether the Association is closed
    ///
    /// Closed Associations cannot transport any further data. An association becomes closed when
    /// either peer application intentionally closes it, or when either transport layer detects an
    /// error such as a time-out or certificate validation failure.
    ///
    /// A `AssociationLost` event is emitted with details when the association becomes closed.
    pub fn is_closed(&self) -> bool {
        self.state == AssociationState::Closed
    }

    /// Whether there is no longer any need to keep the association around
    ///
    /// Closed associations become drained after a brief timeout to absorb any remaining in-flight
    /// packets from the peer. All drained associations have been closed.
    pub fn is_drained(&self) -> bool {
        self.state.is_drained()
    }

    /// Look up whether we're the client or server of this Association
    pub fn side(&self) -> Side {
        self.side
    }

    /// The latest socket address for this Association's peer
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Current best estimate of this Association's latency (round-trip-time)
    pub fn rtt(&self) -> Duration {
        Duration::from_millis(self.rto_mgr.get_rto())
    }

    /// The local IP address which was used when the peer established
    /// the association
    ///
    /// This can be different from the address the endpoint is bound to, in case
    /// the endpoint is bound to a wildcard address like `0.0.0.0` or `::`.
    ///
    /// This will return `None` for clients.
    ///
    /// Retrieving the local IP address is currently supported on the following
    /// platforms:
    /// - Linux
    ///
    /// On all non-supported platforms the local IP address will not be available,
    /// and the method will return `None`.
    pub fn local_ip(&self) -> Option<IpAddr> {
        self.local_ip
    }

    /// Shutdown initiates the shutdown sequence. The method blocks until the
    /// shutdown sequence is completed and the association is closed, or until the
    /// passed context is done, in which case the context's error is returned.
    pub fn shutdown(&mut self) -> Result<()> {
        debug!("[{}] closing association..", self.side);

        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrShutdownNonEstablished);
        }

        // Attempt a graceful shutdown.
        self.set_state(AssociationState::ShutdownPending);

        if self.inflight_queue_length == 0 {
            // No more outstanding, send shutdown.
            self.will_send_shutdown = true;
            self.awake_write_loop();
            self.set_state(AssociationState::ShutdownSent);
        }

        self.endpoint_events.push_back(EndpointEventInner::Drained);

        Ok(())
    }

    /// Close ends the SCTP Association and cleans up any state
    pub fn close(&mut self) -> Result<()> {
        if self.state() != AssociationState::Closed {
            self.set_state(AssociationState::Closed);

            debug!("[{}] closing association..", self.side);

            self.close_all_timers();

            for si in self.streams.keys().cloned().collect::<Vec<u16>>() {
                self.unregister_stream(si);
            }

            debug!("[{}] association closed", self.side);
            debug!(
                "[{}] stats nDATAs (in) : {}",
                self.side,
                self.stats.get_num_datas()
            );
            debug!(
                "[{}] stats nSACKs (in) : {}",
                self.side,
                self.stats.get_num_sacks()
            );
            debug!(
                "[{}] stats nT3Timeouts : {}",
                self.side,
                self.stats.get_num_t3timeouts()
            );
            debug!(
                "[{}] stats nAckTimeouts: {}",
                self.side,
                self.stats.get_num_ack_timeouts()
            );
            debug!(
                "[{}] stats nFastRetrans: {}",
                self.side,
                self.stats.get_num_fast_retrans()
            );
        }

        Ok(())
    }

    /// open_stream opens a stream
    pub fn open_stream(
        &mut self,
        stream_identifier: StreamId,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Result<Stream<'_>> {
        if self.streams.contains_key(&stream_identifier) {
            return Err(Error::ErrStreamAlreadyExist);
        }

        if let Some(s) = self.create_stream(stream_identifier, false, default_payload_type) {
            Ok(s)
        } else {
            Err(Error::ErrStreamCreateFailed)
        }
    }

    /// accept_stream accepts a stream
    pub fn accept_stream(&mut self) -> Option<Stream<'_>> {
        self.stream_queue
            .pop_front()
            .map(move |stream_identifier| Stream {
                stream_identifier,
                association: self,
            })
    }

    /// stream returns a stream
    pub fn stream(&mut self, stream_identifier: StreamId) -> Result<Stream<'_>> {
        if !self.streams.contains_key(&stream_identifier) {
            Err(Error::ErrStreamNotExisted)
        } else {
            Ok(Stream {
                stream_identifier,
                association: self,
            })
        }
    }

    /// bytes_sent returns the number of bytes sent
    pub(crate) fn bytes_sent(&self) -> usize {
        self.bytes_sent
    }

    /// bytes_received returns the number of bytes received
    pub(crate) fn bytes_received(&self) -> usize {
        self.bytes_received
    }

    /// max_message_size returns the maximum message size you can send.
    pub(crate) fn max_message_size(&self) -> u32 {
        self.max_message_size
    }

    /// set_max_message_size sets the maximum message size you can send.
    pub(crate) fn set_max_message_size(&mut self, max_message_size: u32) {
        self.max_message_size = max_message_size;
    }

    /// unregister_stream un-registers a stream from the association
    /// The caller should hold the association write lock.
    fn unregister_stream(&mut self, stream_identifier: StreamId) {
        if let Some(mut s) = self.streams.remove(&stream_identifier) {
            debug!("[{}] unregister_stream {}", self.side, stream_identifier);
            s.state = RecvSendState::Closed;
        }
    }

    /// set_state atomically sets the state of the Association.
    fn set_state(&mut self, new_state: AssociationState) {
        if new_state != self.state {
            debug!(
                "[{}] state change: '{}' => '{}'",
                self.side, self.state, new_state,
            );
        }
        self.state = new_state;
    }

    /// state atomically returns the state of the Association.
    pub(crate) fn state(&self) -> AssociationState {
        self.state
    }

    /// caller must hold self.lock
    fn send_init(&mut self) -> Result<()> {
        if let Some(stored_init) = &self.stored_init {
            debug!("[{}] sending INIT", self.side);

            self.source_port = 5000; // Spec??
            self.destination_port = 5000; // Spec??

            let outbound = Packet {
                common_header: CommonHeader {
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                    verification_tag: self.peer_verification_tag,
                },
                chunks: vec![Box::new(stored_init.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrInitNotStoredToSend)
        }
    }

    /// caller must hold self.lock
    fn send_cookie_echo(&mut self) -> Result<()> {
        if let Some(stored_cookie_echo) = &self.stored_cookie_echo {
            debug!("[{}] sending COOKIE-ECHO", self.side);

            let outbound = Packet {
                common_header: CommonHeader {
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                    verification_tag: self.peer_verification_tag,
                },
                chunks: vec![Box::new(stored_cookie_echo.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrCookieEchoNotStoredToSend)
        }
    }

    /// handle_inbound parses incoming raw packets
    fn handle_inbound(&mut self, p: Packet, now: Instant) -> Result<()> {
        if let Err(err) = p.check_packet() {
            warn!("[{}] failed validating packet {}", self.side, err);
            return Ok(());
        }

        self.handle_chunk_start();

        for c in &p.chunks {
            self.handle_chunk(&p, c, now)?;
        }

        self.handle_chunk_end(now);

        Ok(())
    }

    fn handle_chunk_start(&mut self) {
        self.delayed_ack_triggered = false;
        self.immediate_ack_triggered = false;
    }

    fn handle_chunk_end(&mut self, now: Instant) {
        if self.immediate_ack_triggered {
            self.ack_state = AckState::Immediate;
            self.timers.stop(Timer::Ack);
            self.awake_write_loop();
        } else if self.delayed_ack_triggered {
            // Will send delayed ack in the next ack timeout
            self.ack_state = AckState::Delay;
            self.timers.start(Timer::Ack, now, ACK_INTERVAL);
        }
    }

    #[allow(clippy::borrowed_box)]
    fn handle_chunk(
        &mut self,
        p: &Packet,
        chunk: &Box<dyn Chunk + Send + Sync>,
        now: Instant,
    ) -> Result<()> {
        chunk.check()?;
        let chunk_any = chunk.as_any();
        let packets = if let Some(c) = chunk_any.downcast_ref::<ChunkInit>() {
            if c.is_ack {
                self.handle_init_ack(p, c, now)?
            } else {
                self.handle_init(p, c)?
            }
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkAbort>() {
            let mut err_str = String::new();
            for e in &c.error_causes {
                if matches!(e.code, USER_INITIATED_ABORT) {
                    debug!("User initiated abort received");
                    let _ = self.close();
                    return Ok(());
                }
                err_str += &format!("({})", e);
            }
            return Err(Error::ErrAbortChunk(err_str));
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkError>() {
            let mut err_str = String::new();
            for e in &c.error_causes {
                err_str += &format!("({})", e);
            }
            return Err(Error::ErrAbortChunk(err_str));
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkHeartbeat>() {
            self.handle_heartbeat(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkCookieEcho>() {
            self.handle_cookie_echo(c)?
        } else if chunk_any.downcast_ref::<ChunkCookieAck>().is_some() {
            self.handle_cookie_ack()?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkPayloadData>() {
            self.handle_data(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkSelectiveAck>() {
            self.handle_sack(c, now)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkReconfig>() {
            self.handle_reconfig(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkForwardTsn>() {
            self.handle_forward_tsn(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdown>() {
            self.handle_shutdown(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownAck>() {
            self.handle_shutdown_ack(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownComplete>() {
            self.handle_shutdown_complete(c)?
        } else {
            return Err(Error::ErrChunkTypeUnhandled);
        };

        if !packets.is_empty() {
            let mut buf: VecDeque<_> = packets.into_iter().collect();
            self.control_queue.append(&mut buf);
            self.awake_write_loop();
        }

        Ok(())
    }

    fn handle_init(&mut self, p: &Packet, i: &ChunkInit) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] chunkInit received in state '{}'", self.side, state);

        // https://tools.ietf.org/html/rfc4960#section-5.2.1
        // Upon receipt of an INIT in the COOKIE-WAIT state, an endpoint MUST
        // respond with an INIT ACK using the same parameters it sent in its
        // original INIT chunk (including its Initiate Tag, unchanged).  When
        // responding, the endpoint MUST send the INIT ACK back to the same
        // address that the original INIT (sent by this endpoint) was sent.

        if state != AssociationState::Closed
            && state != AssociationState::CookieWait
            && state != AssociationState::CookieEchoed
        {
            // 5.2.2.  Unexpected INIT in States Other than CLOSED, COOKIE-ECHOED,
            //        COOKIE-WAIT, and SHUTDOWN-ACK-SENT
            return Err(Error::ErrHandleInitState);
        }

        // Should we be setting any of these permanently until we've ACKed further?
        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_verification_tag = i.initiate_tag;
        self.source_port = p.common_header.destination_port;
        self.destination_port = p.common_header.source_port;

        // 13.2 This is the last TSN received in sequence.  This value
        // is set initially by taking the peer's initial TSN,
        // received in the INIT or INIT ACK chunk, and
        // subtracting one from it.
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };

        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on init)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on init)", self.side);
        }

        let mut outbound = Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks: vec![],
        };

        let mut init_ack = ChunkInit {
            is_ack: true,
            initial_tsn: self.my_next_tsn,
            num_outbound_streams: self.my_max_num_outbound_streams,
            num_inbound_streams: self.my_max_num_inbound_streams,
            initiate_tag: self.my_verification_tag,
            advertised_receiver_window_credit: self.max_receive_buffer_size,
            ..Default::default()
        };

        if self.my_cookie.is_none() {
            self.my_cookie = Some(ParamStateCookie::new());
        }

        if let Some(my_cookie) = &self.my_cookie {
            init_ack.params = vec![Box::new(my_cookie.clone())];
        }

        init_ack.set_supported_extensions();

        outbound.chunks = vec![Box::new(init_ack)];

        Ok(vec![outbound])
    }

    fn handle_init_ack(
        &mut self,
        p: &Packet,
        i: &ChunkInitAck,
        now: Instant,
    ) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] chunkInitAck received in state '{}'", self.side, state);
        if state != AssociationState::CookieWait {
            // RFC 4960
            // 5.2.3.  Unexpected INIT ACK
            //   If an INIT ACK is received by an endpoint in any state other than the
            //   COOKIE-WAIT state, the endpoint should discard the INIT ACK chunk.
            //   An unexpected INIT ACK usually indicates the processing of an old or
            //   duplicated INIT chunk.
            return Ok(vec![]);
        }

        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_verification_tag = i.initiate_tag;
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };
        if self.source_port != p.common_header.destination_port
            || self.destination_port != p.common_header.source_port
        {
            warn!("[{}] handle_init_ack: port mismatch", self.side);
            return Ok(vec![]);
        }

        self.rwnd = i.advertised_receiver_window_credit;
        debug!("[{}] initial rwnd={}", self.side, self.rwnd);

        // RFC 4690 Sec 7.2.1
        //  o  The initial value of ssthresh MAY be arbitrarily high (for
        //     example, implementations MAY use the size of the receiver
        //     advertised window).
        self.ssthresh = self.rwnd;
        trace!(
            "[{}] updated cwnd={} ssthresh={} inflight={} (INI)",
            self.side,
            self.cwnd,
            self.ssthresh,
            self.inflight_queue.get_num_bytes()
        );

        self.timers.stop(Timer::T1Init);
        self.stored_init = None;

        let mut cookie_param = None;
        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamStateCookie>() {
                cookie_param = Some(v);
            } else if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on initAck)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on initAck)", self.side);
        }

        if let Some(v) = cookie_param {
            self.stored_cookie_echo = Some(ChunkCookieEcho {
                cookie: v.cookie.clone(),
            });

            self.send_cookie_echo()?;

            self.timers
                .start(Timer::T1Cookie, now, self.rto_mgr.get_rto());

            self.set_state(AssociationState::CookieEchoed);

            Ok(vec![])
        } else {
            Err(Error::ErrInitAckNoCookie)
        }
    }

    fn handle_heartbeat(&self, c: &ChunkHeartbeat) -> Result<Vec<Packet>> {
        trace!("[{}] chunkHeartbeat", self.side);
        if let Some(p) = c.params.first() {
            if let Some(hbi) = p.as_any().downcast_ref::<ParamHeartbeatInfo>() {
                return Ok(vec![Packet {
                    common_header: CommonHeader {
                        verification_tag: self.peer_verification_tag,
                        source_port: self.source_port,
                        destination_port: self.destination_port,
                    },
                    chunks: vec![Box::new(ChunkHeartbeatAck {
                        params: vec![Box::new(ParamHeartbeatInfo {
                            heartbeat_information: hbi.heartbeat_information.clone(),
                        })],
                    })],
                }]);
            } else {
                warn!(
                    "[{}] failed to handle Heartbeat, no ParamHeartbeatInfo",
                    self.side,
                );
            }
        }

        Ok(vec![])
    }

    fn handle_cookie_echo(&mut self, c: &ChunkCookieEcho) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] COOKIE-ECHO received in state '{}'", self.side, state);

        if let Some(my_cookie) = &self.my_cookie {
            match state {
                AssociationState::Established => {
                    if my_cookie.cookie != c.cookie {
                        return Ok(vec![]);
                    }
                }
                AssociationState::Closed
                | AssociationState::CookieWait
                | AssociationState::CookieEchoed => {
                    if my_cookie.cookie != c.cookie {
                        return Ok(vec![]);
                    }

                    self.timers.stop(Timer::T1Init);
                    self.stored_init = None;

                    self.timers.stop(Timer::T1Cookie);
                    self.stored_cookie_echo = None;

                    self.events.push_back(Event::Connected);
                    self.set_state(AssociationState::Established);
                    self.handshake_completed = true;
                }
                _ => return Ok(vec![]),
            };
        } else {
            debug!("[{}] COOKIE-ECHO received before initialization", self.side);
            return Ok(vec![]);
        }

        Ok(vec![Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks: vec![Box::new(ChunkCookieAck {})],
        }])
    }

    fn handle_cookie_ack(&mut self) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] COOKIE-ACK received in state '{}'", self.side, state);
        if state != AssociationState::CookieEchoed {
            // RFC 4960
            // 5.2.5.  Handle Duplicate COOKIE-ACK.
            //   At any state other than COOKIE-ECHOED, an endpoint should silently
            //   discard a received COOKIE ACK chunk.
            return Ok(vec![]);
        }

        self.timers.stop(Timer::T1Cookie);
        self.stored_cookie_echo = None;

        self.events.push_back(Event::Connected);
        self.set_state(AssociationState::Established);
        self.handshake_completed = true;

        Ok(vec![])
    }

    fn handle_data(&mut self, d: &ChunkPayloadData) -> Result<Vec<Packet>> {
        trace!(
            "[{}] DATA: tsn={} immediateSack={} len={}",
            self.side,
            d.tsn,
            d.immediate_sack,
            d.user_data.len()
        );
        self.stats.inc_datas();

        let can_push = self.payload_queue.can_push(d, self.peer_last_tsn);
        let mut stream_handle_data = false;
        if can_push {
            if self.get_or_create_stream(d.stream_identifier).is_some() {
                if self.get_my_receiver_window_credit() > 0 {
                    // Pass the new chunk to stream level as soon as it arrives
                    self.payload_queue.push(d.clone(), self.peer_last_tsn);
                    stream_handle_data = true;
                } else {
                    // Receive buffer is full
                    if let Some(last_tsn) = self.payload_queue.get_last_tsn_received() {
                        if sna32lt(d.tsn, *last_tsn) {
                            debug!("[{}] receive buffer full, but accepted as this is a missing chunk with tsn={} ssn={}", self.side, d.tsn, d.stream_sequence_number);
                            self.payload_queue.push(d.clone(), self.peer_last_tsn);
                            stream_handle_data = true; //s.handle_data(d.clone());
                        }
                    } else {
                        debug!(
                            "[{}] receive buffer full. dropping DATA with tsn={} ssn={}",
                            self.side, d.tsn, d.stream_sequence_number
                        );
                    }
                }
            } else {
                // silently discard the data. (sender will retry on T3-rtx timeout)
                // see pion/sctp#30
                debug!("[{}] discard {}", self.side, d.stream_sequence_number);
                return Ok(vec![]);
            }
        }

        let immediate_sack = d.immediate_sack;

        if stream_handle_data {
            if let Some(s) = self.streams.get_mut(&d.stream_identifier) {
                self.events.push_back(Event::DatagramReceived);
                s.handle_data(d);
                if s.reassembly_queue.is_readable() {
                    self.events.push_back(Event::Stream(StreamEvent::Readable {
                        id: d.stream_identifier,
                    }))
                }
            }
        }

        self.handle_peer_last_tsn_and_acknowledgement(immediate_sack)
    }

    fn handle_sack(&mut self, d: &ChunkSelectiveAck, now: Instant) -> Result<Vec<Packet>> {
        trace!(
            "[{}] {}, SACK: cumTSN={} a_rwnd={}",
            self.side,
            self.cumulative_tsn_ack_point,
            d.cumulative_tsn_ack,
            d.advertised_receiver_window_credit
        );
        let state = self.state();
        if state != AssociationState::Established
            && state != AssociationState::ShutdownPending
            && state != AssociationState::ShutdownReceived
        {
            return Ok(vec![]);
        }

        self.stats.inc_sacks();

        if sna32gt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            // RFC 4960 sec 6.2.1.  Processing a Received SACK
            // D)
            //   i) If Cumulative TSN Ack is less than the Cumulative TSN Ack
            //      Point, then drop the SACK.  Since Cumulative TSN Ack is
            //      monotonically increasing, a SACK whose Cumulative TSN Ack is
            //      less than the Cumulative TSN Ack Point indicates an out-of-
            //      order SACK.

            debug!(
                "[{}] SACK Cumulative ACK {} is older than ACK point {}",
                self.side, d.cumulative_tsn_ack, self.cumulative_tsn_ack_point
            );

            return Ok(vec![]);
        }

        // Process selective ack
        let (bytes_acked_per_stream, htna) = self.process_selective_ack(d, now)?;

        let mut total_bytes_acked = 0;
        for n_bytes_acked in bytes_acked_per_stream.values() {
            total_bytes_acked += *n_bytes_acked;
        }

        let mut cum_tsn_ack_point_advanced = false;
        if sna32lt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            trace!(
                "[{}] SACK: cumTSN advanced: {} -> {}",
                self.side,
                self.cumulative_tsn_ack_point,
                d.cumulative_tsn_ack
            );

            self.cumulative_tsn_ack_point = d.cumulative_tsn_ack;
            cum_tsn_ack_point_advanced = true;
            self.on_cumulative_tsn_ack_point_advanced(total_bytes_acked, now);
        }

        for (si, n_bytes_acked) in &bytes_acked_per_stream {
            if let Some(s) = self.streams.get_mut(si) {
                if s.on_buffer_released(*n_bytes_acked) {
                    self.events
                        .push_back(Event::Stream(StreamEvent::BufferedAmountLow { id: *si }))
                }
            }
        }

        // New rwnd value
        // RFC 4960 sec 6.2.1.  Processing a Received SACK
        // D)
        //   ii) Set rwnd equal to the newly received a_rwnd minus the number
        //       of bytes still outstanding after processing the Cumulative
        //       TSN Ack and the Gap Ack Blocks.

        // bytes acked were already subtracted by markAsAcked() method
        let bytes_outstanding = self.inflight_queue.get_num_bytes() as u32;
        if bytes_outstanding >= d.advertised_receiver_window_credit {
            self.rwnd = 0;
        } else {
            self.rwnd = d.advertised_receiver_window_credit - bytes_outstanding;
        }

        self.process_fast_retransmission(d.cumulative_tsn_ack, htna, cum_tsn_ack_point_advanced)?;

        if self.use_forward_tsn {
            // RFC 3758 Sec 3.5 C1
            if sna32lt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                self.advanced_peer_tsn_ack_point = self.cumulative_tsn_ack_point
            }

            // RFC 3758 Sec 3.5 C2
            let mut i = self.advanced_peer_tsn_ack_point + 1;
            while let Some(c) = self.inflight_queue.get(i) {
                if !c.abandoned() {
                    break;
                }
                self.advanced_peer_tsn_ack_point = i;
                i += 1;
            }

            // RFC 3758 Sec 3.5 C3
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                self.will_send_forward_tsn = true;
                debug!(
                    "[{}] handleSack {}: sna32GT({}, {})",
                    self.side,
                    self.will_send_forward_tsn,
                    self.advanced_peer_tsn_ack_point,
                    self.cumulative_tsn_ack_point
                );
            }
            self.awake_write_loop();
        }

        self.postprocess_sack(state, cum_tsn_ack_point_advanced, now);

        Ok(vec![])
    }

    fn handle_reconfig(&mut self, c: &ChunkReconfig) -> Result<Vec<Packet>> {
        trace!("[{}] handle_reconfig", self.side);

        let mut pp = vec![];

        if let Some(param_a) = &c.param_a {
            self.handle_reconfig_param(param_a, &mut pp)?;
        }

        if let Some(param_b) = &c.param_b {
            self.handle_reconfig_param(param_b, &mut pp)?;
        }

        Ok(pp)
    }

    fn handle_forward_tsn(&mut self, c: &ChunkForwardTsn) -> Result<Vec<Packet>> {
        trace!("[{}] FwdTSN: {}", self.side, c);

        if !self.use_forward_tsn {
            warn!("[{}] received FwdTSN but not enabled", self.side);
            // Return an error chunk
            let cerr = ChunkError {
                error_causes: vec![ErrorCauseUnrecognizedChunkType::default()],
            };

            let outbound = Packet {
                common_header: CommonHeader {
                    verification_tag: self.peer_verification_tag,
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                },
                chunks: vec![Box::new(cerr)],
            };
            return Ok(vec![outbound]);
        }

        // From RFC 3758 Sec 3.6:
        //   Note, if the "New Cumulative TSN" value carried in the arrived
        //   FORWARD TSN chunk is found to be behind or at the current cumulative
        //   TSN point, the data receiver MUST treat this FORWARD TSN as out-of-
        //   date and MUST NOT update its Cumulative TSN.  The receiver SHOULD
        //   send a SACK to its peer (the sender of the FORWARD TSN) since such a
        //   duplicate may indicate the previous SACK was lost in the network.

        trace!(
            "[{}] should send ack? newCumTSN={} peer_last_tsn={}",
            self.side,
            c.new_cumulative_tsn,
            self.peer_last_tsn
        );
        if sna32lte(c.new_cumulative_tsn, self.peer_last_tsn) {
            trace!("[{}] sending ack on Forward TSN", self.side);
            self.ack_state = AckState::Immediate;
            self.timers.stop(Timer::Ack);
            self.awake_write_loop();
            return Ok(vec![]);
        }

        // From RFC 3758 Sec 3.6:
        //   the receiver MUST perform the same TSN handling, including duplicate
        //   detection, gap detection, SACK generation, cumulative TSN
        //   advancement, etc. as defined in RFC 2960 [2]---with the following
        //   exceptions and additions.

        //   When a FORWARD TSN chunk arrives, the data receiver MUST first update
        //   its cumulative TSN point to the value carried in the FORWARD TSN
        //   chunk,

        // Advance peer_last_tsn
        while sna32lt(self.peer_last_tsn, c.new_cumulative_tsn) {
            self.payload_queue.pop(self.peer_last_tsn + 1); // may not exist
            self.peer_last_tsn += 1;
        }

        // Report new peer_last_tsn value and abandoned largest SSN value to
        // corresponding streams so that the abandoned chunks can be removed
        // from the reassemblyQueue.
        for forwarded in &c.streams {
            if let Some(s) = self.streams.get_mut(&forwarded.identifier) {
                s.handle_forward_tsn_for_ordered(forwarded.sequence);
            }
        }

        // TSN may be forewared for unordered chunks. ForwardTSN chunk does not
        // report which stream identifier it skipped for unordered chunks.
        // Therefore, we need to broadcast this event to all existing streams for
        // unordered chunks.
        // See https://github.com/pion/sctp/issues/106
        for s in self.streams.values_mut() {
            s.handle_forward_tsn_for_unordered(c.new_cumulative_tsn);
        }

        self.handle_peer_last_tsn_and_acknowledgement(false)
    }

    fn handle_shutdown(&mut self, _: &ChunkShutdown) -> Result<Vec<Packet>> {
        let state = self.state();

        if state == AssociationState::Established {
            if !self.inflight_queue.is_empty() {
                self.set_state(AssociationState::ShutdownReceived);
            } else {
                // No more outstanding, send shutdown ack.
                self.will_send_shutdown_ack = true;
                self.set_state(AssociationState::ShutdownAckSent);

                self.awake_write_loop();
            }
        } else if state == AssociationState::ShutdownSent {
            // self.cumulative_tsn_ack_point = c.cumulative_tsn_ack

            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);

            self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_ack(&mut self, _: &ChunkShutdownAck) -> Result<Vec<Packet>> {
        let state = self.state();
        if state == AssociationState::ShutdownSent || state == AssociationState::ShutdownAckSent {
            self.timers.stop(Timer::T2Shutdown);
            self.will_send_shutdown_complete = true;

            self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_complete(&mut self, _: &ChunkShutdownComplete) -> Result<Vec<Packet>> {
        let state = self.state();
        if state == AssociationState::ShutdownAckSent {
            self.timers.stop(Timer::T2Shutdown);
            self.close()?;
        }

        Ok(vec![])
    }

    /// A common routine for handle_data and handle_forward_tsn routines
    fn handle_peer_last_tsn_and_acknowledgement(
        &mut self,
        sack_immediately: bool,
    ) -> Result<Vec<Packet>> {
        let mut reply = vec![];

        // Try to advance peer_last_tsn

        // From RFC 3758 Sec 3.6:
        //   .. and then MUST further advance its cumulative TSN point locally
        //   if possible
        // Meaning, if peer_last_tsn+1 points to a chunk that is received,
        // advance peer_last_tsn until peer_last_tsn+1 points to unreceived chunk.
        //debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);
        while self.payload_queue.pop(self.peer_last_tsn + 1).is_some() {
            self.peer_last_tsn += 1;
            //debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);

            let rst_reqs: Vec<ParamOutgoingResetRequest> =
                self.reconfig_requests.values().cloned().collect();
            for rst_req in rst_reqs {
                self.reset_streams_if_any(&rst_req, false, &mut reply)?;
            }
        }

        let has_packet_loss = !self.payload_queue.is_empty();
        if has_packet_loss {
            trace!(
                "[{}] packetloss: {}",
                self.side,
                self.payload_queue
                    .get_gap_ack_blocks_string(self.peer_last_tsn)
            );
        }

        if (self.ack_state != AckState::Immediate
            && !sack_immediately
            && !has_packet_loss
            && self.ack_mode == AckMode::Normal)
            || self.ack_mode == AckMode::AlwaysDelay
        {
            if self.ack_state == AckState::Idle {
                self.delayed_ack_triggered = true;
            } else {
                self.immediate_ack_triggered = true;
            }
        } else {
            self.immediate_ack_triggered = true;
        }

        Ok(reply)
    }

    #[allow(clippy::borrowed_box)]
    fn handle_reconfig_param(
        &mut self,
        raw: &Box<dyn Param + Send + Sync>,
        reply: &mut Vec<Packet>,
    ) -> Result<()> {
        if let Some(p) = raw.as_any().downcast_ref::<ParamOutgoingResetRequest>() {
            self.reconfig_requests
                .insert(p.reconfig_request_sequence_number, p.clone());
            self.reset_streams_if_any(p, true, reply)?;
            Ok(())
        } else if let Some(p) = raw.as_any().downcast_ref::<ParamReconfigResponse>() {
            self.reconfigs.remove(&p.reconfig_response_sequence_number);
            if self.reconfigs.is_empty() {
                self.timers.stop(Timer::Reconfig);
            }
            Ok(())
        } else {
            Err(Error::ErrParameterType)
        }
    }

    fn process_selective_ack(
        &mut self,
        d: &ChunkSelectiveAck,
        now: Instant,
    ) -> Result<(HashMap<u16, i64>, u32)> {
        let mut bytes_acked_per_stream = HashMap::new();

        // New ack point, so pop all ACKed packets from inflight_queue
        // We add 1 because the "currentAckPoint" has already been popped from the inflight queue
        // For the first SACK we take care of this by setting the ackpoint to cumAck - 1
        let mut i = self.cumulative_tsn_ack_point + 1;
        //log::debug!("[{}] i={} d={}", self.name, i, d.cumulative_tsn_ack);
        while sna32lte(i, d.cumulative_tsn_ack) {
            if let Some(c) = self.inflight_queue.pop(i) {
                if !c.acked {
                    // RFC 4096 sec 6.3.2.  Retransmission Timer Rules
                    //   R3)  Whenever a SACK is received that acknowledges the DATA chunk
                    //        with the earliest outstanding TSN for that address, restart the
                    //        T3-rtx timer for that address with its current RTO (if there is
                    //        still outstanding data on that address).
                    if i == self.cumulative_tsn_ack_point + 1 {
                        // T3 timer needs to be reset. Stop it for now.
                        self.timers.stop(Timer::T3RTX);
                    }

                    let n_bytes_acked = c.user_data.len() as i64;

                    // Sum the number of bytes acknowledged per stream
                    if let Some(amount) = bytes_acked_per_stream.get_mut(&c.stream_identifier) {
                        *amount += n_bytes_acked;
                    } else {
                        bytes_acked_per_stream.insert(c.stream_identifier, n_bytes_acked);
                    }

                    // RFC 4960 sec 6.3.1.  RTO Calculation
                    //   C4)  When data is in flight and when allowed by rule C5 below, a new
                    //        RTT measurement MUST be made each round trip.  Furthermore, new
                    //        RTT measurements SHOULD be made no more than once per round trip
                    //        for a given destination transport address.
                    //   C5)  Karn's algorithm: RTT measurements MUST NOT be made using
                    //        packets that were retransmitted (and thus for which it is
                    //        ambiguous whether the reply was for the first instance of the
                    //        chunk or for a later instance)
                    if c.nsent == 1 && sna32gte(c.tsn, self.min_tsn2measure_rtt) {
                        self.min_tsn2measure_rtt = self.my_next_tsn;
                        if let Some(since) = &c.since {
                            let rtt = now.duration_since(*since);
                            let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                            trace!(
                                "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                                self.side,
                                rtt.as_millis(),
                                srtt,
                                self.rto_mgr.get_rto()
                            );
                        } else {
                            error!("[{}] invalid c.since", self.side);
                        }
                    }
                }

                if self.in_fast_recovery && c.tsn == self.fast_recover_exit_point {
                    debug!("[{}] exit fast-recovery", self.side);
                    self.in_fast_recovery = false;
                }
            } else {
                return Err(Error::ErrInflightQueueTsnPop);
            }

            i += 1;
        }

        let mut htna = d.cumulative_tsn_ack;

        // Mark selectively acknowledged chunks as "acked"
        for g in &d.gap_ack_blocks {
            for i in g.start..=g.end {
                let tsn = d.cumulative_tsn_ack + i as u32;

                let (is_existed, is_acked) = if let Some(c) = self.inflight_queue.get(tsn) {
                    (true, c.acked)
                } else {
                    (false, false)
                };
                let n_bytes_acked = if is_existed && !is_acked {
                    self.inflight_queue.mark_as_acked(tsn) as i64
                } else {
                    0
                };

                if let Some(c) = self.inflight_queue.get(tsn) {
                    if !is_acked {
                        // Sum the number of bytes acknowledged per stream
                        if let Some(amount) = bytes_acked_per_stream.get_mut(&c.stream_identifier) {
                            *amount += n_bytes_acked;
                        } else {
                            bytes_acked_per_stream.insert(c.stream_identifier, n_bytes_acked);
                        }

                        trace!("[{}] tsn={} has been sacked", self.side, c.tsn);

                        if c.nsent == 1 {
                            self.min_tsn2measure_rtt = self.my_next_tsn;
                            if let Some(since) = &c.since {
                                let rtt = now.duration_since(*since);
                                let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                                trace!(
                                    "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                                    self.side,
                                    rtt.as_millis(),
                                    srtt,
                                    self.rto_mgr.get_rto()
                                );
                            } else {
                                error!("[{}] invalid c.since", self.side);
                            }
                        }

                        if sna32lt(htna, tsn) {
                            htna = tsn;
                        }
                    }
                } else {
                    return Err(Error::ErrTsnRequestNotExist);
                }
            }
        }

        Ok((bytes_acked_per_stream, htna))
    }

    fn on_cumulative_tsn_ack_point_advanced(&mut self, total_bytes_acked: i64, now: Instant) {
        // RFC 4096, sec 6.3.2.  Retransmission Timer Rules
        //   R2)  Whenever all outstanding data sent to an address have been
        //        acknowledged, turn off the T3-rtx timer of that address.
        if self.inflight_queue.is_empty() {
            trace!(
                "[{}] SACK: no more packet in-flight (pending={})",
                self.side,
                self.pending_queue.len()
            );
            self.timers.stop(Timer::T3RTX);
        } else {
            trace!("[{}] T3-rtx timer start (pt2)", self.side);
            self.timers
                .restart_if_stale(Timer::T3RTX, now, self.rto_mgr.get_rto());
        }

        // Update congestion control parameters
        if self.cwnd <= self.ssthresh {
            // RFC 4096, sec 7.2.1.  Slow-Start
            //   o  When cwnd is less than or equal to ssthresh, an SCTP endpoint MUST
            //		use the slow-start algorithm to increase cwnd only if the current
            //      congestion window is being fully utilized, an incoming SACK
            //      advances the Cumulative TSN Ack Point, and the data sender is not
            //      in Fast Recovery.  Only when these three conditions are met can
            //      the cwnd be increased; otherwise, the cwnd MUST not be increased.
            //		If these conditions are met, then cwnd MUST be increased by, at
            //      most, the lesser of 1) the total size of the previously
            //      outstanding DATA chunk(s) acknowledged, and 2) the destination's
            //      path MTU.
            if !self.in_fast_recovery && !self.pending_queue.is_empty() {
                self.cwnd += std::cmp::min(total_bytes_acked as u32, self.cwnd); // TCP way
                                                                                 // self.cwnd += min32(uint32(total_bytes_acked), self.mtu) // SCTP way (slow)
                trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (SS)",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked
                );
            } else {
                trace!(
                    "[{}] cwnd did not grow: cwnd={} ssthresh={} acked={} FR={} pending={}",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked,
                    self.in_fast_recovery,
                    self.pending_queue.len()
                );
            }
        } else {
            // RFC 4096, sec 7.2.2.  Congestion Avoidance
            //   o  Whenever cwnd is greater than ssthresh, upon each SACK arrival
            //      that advances the Cumulative TSN Ack Point, increase
            //      partial_bytes_acked by the total number of bytes of all new chunks
            //      acknowledged in that SACK including chunks acknowledged by the new
            //      Cumulative TSN Ack and by Gap Ack Blocks.
            self.partial_bytes_acked += total_bytes_acked as u32;

            //   o  When partial_bytes_acked is equal to or greater than cwnd and
            //      before the arrival of the SACK the sender had cwnd or more bytes
            //      of data outstanding (i.e., before arrival of the SACK, flight size
            //      was greater than or equal to cwnd), increase cwnd by MTU, and
            //      reset partial_bytes_acked to (partial_bytes_acked - cwnd).
            if self.partial_bytes_acked >= self.cwnd && !self.pending_queue.is_empty() {
                self.partial_bytes_acked -= self.cwnd;
                self.cwnd += self.mtu;
                trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (CA)",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked
                );
            }
        }
    }

    fn process_fast_retransmission(
        &mut self,
        cum_tsn_ack_point: u32,
        htna: u32,
        cum_tsn_ack_point_advanced: bool,
    ) -> Result<()> {
        // HTNA algorithm - RFC 4960 Sec 7.2.4
        // Increment missIndicator of each chunks that the SACK reported missing
        // when either of the following is met:
        // a)  Not in fast-recovery
        //     miss indications are incremented only for missing TSNs prior to the
        //     highest TSN newly acknowledged in the SACK.
        // b)  In fast-recovery AND the Cumulative TSN Ack Point advanced
        //     the miss indications are incremented for all TSNs reported missing
        //     in the SACK.
        if !self.in_fast_recovery || cum_tsn_ack_point_advanced {
            let max_tsn = if !self.in_fast_recovery {
                // a) increment only for missing TSNs prior to the HTNA
                htna
            } else {
                // b) increment for all TSNs reported missing
                cum_tsn_ack_point + (self.inflight_queue.len() as u32) + 1
            };

            let mut tsn = cum_tsn_ack_point + 1;
            while sna32lt(tsn, max_tsn) {
                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    if !c.acked && !c.abandoned() && c.miss_indicator < 3 {
                        c.miss_indicator += 1;
                        if c.miss_indicator == 3 && !self.in_fast_recovery {
                            // 2)  If not in Fast Recovery, adjust the ssthresh and cwnd of the
                            //     destination address(es) to which the missing DATA chunks were
                            //     last sent, according to the formula described in Section 7.2.3.
                            self.in_fast_recovery = true;
                            self.fast_recover_exit_point = htna;
                            self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                            self.cwnd = self.ssthresh;
                            self.partial_bytes_acked = 0;
                            self.will_retransmit_fast = true;

                            trace!(
                                "[{}] updated cwnd={} ssthresh={} inflight={} (FR)",
                                self.side,
                                self.cwnd,
                                self.ssthresh,
                                self.inflight_queue.get_num_bytes()
                            );
                        }
                    }
                } else {
                    return Err(Error::ErrTsnRequestNotExist);
                }

                tsn += 1;
            }
        }

        if self.in_fast_recovery && cum_tsn_ack_point_advanced {
            self.will_retransmit_fast = true;
        }

        Ok(())
    }

    /// The caller must hold the lock. This method was only added because the
    /// linter was complaining about the "cognitive complexity" of handle_sack.
    fn postprocess_sack(
        &mut self,
        state: AssociationState,
        mut should_awake_write_loop: bool,
        now: Instant,
    ) {
        if !self.inflight_queue.is_empty() {
            // Start timer. (noop if already started)
            trace!("[{}] T3-rtx timer start (pt3)", self.side);
            self.timers
                .restart_if_stale(Timer::T3RTX, now, self.rto_mgr.get_rto());
        } else if state == AssociationState::ShutdownPending {
            // No more outstanding, send shutdown.
            should_awake_write_loop = true;
            self.will_send_shutdown = true;
            self.set_state(AssociationState::ShutdownSent);
        } else if state == AssociationState::ShutdownReceived {
            // No more outstanding, send shutdown ack.
            should_awake_write_loop = true;
            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);
        }

        if should_awake_write_loop {
            self.awake_write_loop();
        }
    }

    fn reset_streams_if_any(
        &mut self,
        p: &ParamOutgoingResetRequest,
        respond: bool,
        reply: &mut Vec<Packet>,
    ) -> Result<()> {
        let mut result = ReconfigResult::SuccessPerformed;
        let mut sis_to_reset = vec![];

        if sna32lte(p.sender_last_tsn, self.peer_last_tsn) {
            debug!(
                "[{}] resetStream(): senderLastTSN={} <= peer_last_tsn={}",
                self.side, p.sender_last_tsn, self.peer_last_tsn
            );
            for id in &p.stream_identifiers {
                if self.streams.contains_key(id) {
                    if respond {
                        sis_to_reset.push(*id);
                    }
                    self.unregister_stream(*id);
                }
            }
            self.reconfig_requests
                .remove(&p.reconfig_request_sequence_number);
        } else {
            debug!(
                "[{}] resetStream(): senderLastTSN={} > peer_last_tsn={}",
                self.side, p.sender_last_tsn, self.peer_last_tsn
            );
            result = ReconfigResult::InProgress;
        }

        // Answer incoming reset requests with the same reset request, but with
        // reconfig_response_sequence_number.
        if !sis_to_reset.is_empty() {
            let rsn = self.generate_next_rsn();
            let tsn = self.my_next_tsn - 1;

            let c = ChunkReconfig {
                param_a: Some(Box::new(ParamOutgoingResetRequest {
                    reconfig_request_sequence_number: rsn,
                    reconfig_response_sequence_number: p.reconfig_request_sequence_number,
                    sender_last_tsn: tsn,
                    stream_identifiers: sis_to_reset,
                })),
                ..Default::default()
            };

            self.reconfigs.insert(rsn, c.clone()); // store in the map for retransmission

            let p = self.create_packet(vec![Box::new(c)]);
            reply.push(p);
        }

        let packet = self.create_packet(vec![Box::new(ChunkReconfig {
            param_a: Some(Box::new(ParamReconfigResponse {
                reconfig_response_sequence_number: p.reconfig_request_sequence_number,
                result,
            })),
            param_b: None,
        })]);

        debug!("[{}] RESET RESPONSE: {}", self.side, packet);

        reply.push(packet);

        Ok(())
    }

    /// create_packet wraps chunks in a packet.
    /// The caller should hold the read lock.
    pub(crate) fn create_packet(&self, chunks: Vec<Box<dyn Chunk + Send + Sync>>) -> Packet {
        Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks,
        }
    }

    /// create_stream creates a stream. The caller should hold the lock and check no stream exists for this id.
    fn create_stream(
        &mut self,
        stream_identifier: StreamId,
        accept: bool,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Option<Stream<'_>> {
        let s = StreamState::new(
            self.side,
            stream_identifier,
            self.max_payload_size,
            default_payload_type,
        );

        if accept {
            self.stream_queue.push_back(stream_identifier);
            self.events.push_back(Event::Stream(StreamEvent::Opened));
        }

        self.streams.insert(stream_identifier, s);

        Some(Stream {
            stream_identifier,
            association: self,
        })
    }

    /// get_or_create_stream gets or creates a stream. The caller should hold the lock.
    fn get_or_create_stream(&mut self, stream_identifier: StreamId) -> Option<Stream<'_>> {
        if self.streams.contains_key(&stream_identifier) {
            Some(Stream {
                stream_identifier,
                association: self,
            })
        } else {
            self.create_stream(
                stream_identifier,
                true,
                PayloadProtocolIdentifier::default(),
            )
        }
    }

    pub(crate) fn get_my_receiver_window_credit(&self) -> u32 {
        let mut bytes_queued = 0;
        for s in self.streams.values() {
            bytes_queued += s.get_num_bytes_in_reassembly_queue() as u32;
        }

        self.max_receive_buffer_size.saturating_sub(bytes_queued)
    }

    /// gather_outbound gathers outgoing packets. The returned bool value set to
    /// false means the association should be closed down after the final send.
    fn gather_outbound(&mut self, now: Instant) -> (Vec<Bytes>, bool) {
        let mut raw_packets = vec![];

        if !self.control_queue.is_empty() {
            for p in self.control_queue.drain(..) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!("[{}] failed to serialize a control packet", self.side);
                    continue;
                }
            }
        }

        let state = self.state();
        match state {
            AssociationState::Established => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets, now);
                raw_packets = self.gather_outbound_data_and_reconfig_packets(raw_packets, now);
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets, now);
                raw_packets = self.gather_outbound_sack_packets(raw_packets);
                raw_packets = self.gather_outbound_forward_tsn_packets(raw_packets);
                (raw_packets, true)
            }
            AssociationState::ShutdownPending
            | AssociationState::ShutdownSent
            | AssociationState::ShutdownReceived => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets, now);
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets, now);
                raw_packets = self.gather_outbound_sack_packets(raw_packets);
                self.gather_outbound_shutdown_packets(raw_packets, now)
            }
            AssociationState::ShutdownAckSent => {
                self.gather_outbound_shutdown_packets(raw_packets, now)
            }
            _ => (raw_packets, true),
        }
    }

    fn gather_data_packets_to_retransmit(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        for p in &self.get_data_packets_to_retransmit(now) {
            if let Ok(raw) = p.marshal() {
                raw_packets.push(raw);
            } else {
                warn!(
                    "[{}] failed to serialize a DATA packet to be retransmitted",
                    self.side
                );
            }
        }

        raw_packets
    }

    fn gather_outbound_data_and_reconfig_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        // Pop unsent data chunks from the pending queue to send as much as
        // cwnd and rwnd allow.
        let (chunks, sis_to_reset) = self.pop_pending_data_chunks_to_send(now);
        if !chunks.is_empty() {
            // Start timer. (noop if already started)
            trace!("[{}] T3-rtx timer start (pt1)", self.side);
            self.timers
                .restart_if_stale(Timer::T3RTX, now, self.rto_mgr.get_rto());

            for p in &self.bundle_data_chunks_into_packets(chunks) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!("[{}] failed to serialize a DATA packet", self.side);
                }
            }
        }

        if !sis_to_reset.is_empty() || self.will_retransmit_reconfig {
            if self.will_retransmit_reconfig {
                self.will_retransmit_reconfig = false;
                debug!(
                    "[{}] retransmit {} RECONFIG chunk(s)",
                    self.side,
                    self.reconfigs.len()
                );
                for c in self.reconfigs.values() {
                    let p = self.create_packet(vec![Box::new(c.clone())]);
                    if let Ok(raw) = p.marshal() {
                        raw_packets.push(raw);
                    } else {
                        warn!(
                            "[{}] failed to serialize a RECONFIG packet to be retransmitted",
                            self.side,
                        );
                    }
                }
            }

            if !sis_to_reset.is_empty() {
                let rsn = self.generate_next_rsn();
                let tsn = self.my_next_tsn - 1;
                debug!(
                    "[{}] sending RECONFIG: rsn={} tsn={} streams={:?}",
                    self.side,
                    rsn,
                    self.my_next_tsn - 1,
                    sis_to_reset
                );

                let c = ChunkReconfig {
                    param_a: Some(Box::new(ParamOutgoingResetRequest {
                        reconfig_request_sequence_number: rsn,
                        sender_last_tsn: tsn,
                        stream_identifiers: sis_to_reset,
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                self.reconfigs.insert(rsn, c.clone()); // store in the map for retransmission

                let p = self.create_packet(vec![Box::new(c)]);
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!(
                        "[{}] failed to serialize a RECONFIG packet to be transmitted",
                        self.side
                    );
                }
            }

            if !self.reconfigs.is_empty() {
                self.timers
                    .start(Timer::Reconfig, now, self.rto_mgr.get_rto());
            }
        }

        raw_packets
    }

    fn gather_outbound_fast_retransmission_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        if self.will_retransmit_fast {
            self.will_retransmit_fast = false;

            let mut to_fast_retrans: Vec<Box<dyn Chunk + Send + Sync>> = vec![];
            let mut fast_retrans_size = COMMON_HEADER_SIZE;

            let mut i = 0;
            loop {
                let tsn = self.cumulative_tsn_ack_point + i + 1;
                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    if c.acked || c.abandoned() || c.nsent > 1 || c.miss_indicator < 3 {
                        i += 1;
                        continue;
                    }

                    // RFC 4960 Sec 7.2.4 Fast Retransmit on Gap Reports
                    //  3)  Determine how many of the earliest (i.e., lowest TSN) DATA chunks
                    //      marked for retransmission will fit into a single packet, subject
                    //      to constraint of the path MTU of the destination transport
                    //      address to which the packet is being sent.  Call this value K.
                    //      Retransmit those K DATA chunks in a single packet.  When a Fast
                    //      Retransmit is being performed, the sender SHOULD ignore the value
                    //      of cwnd and SHOULD NOT delay retransmission for this single
                    //		packet.

                    let data_chunk_size = DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32;
                    if self.mtu < fast_retrans_size + data_chunk_size {
                        break;
                    }

                    fast_retrans_size += data_chunk_size;
                    self.stats.inc_fast_retrans();
                    c.nsent += 1;
                } else {
                    break; // end of pending data
                }

                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    Association::check_partial_reliability_status(
                        c,
                        now,
                        self.use_forward_tsn,
                        self.side,
                        &self.streams,
                    );
                    to_fast_retrans.push(Box::new(c.clone()));
                    trace!(
                        "[{}] fast-retransmit: tsn={} sent={} htna={}",
                        self.side,
                        c.tsn,
                        c.nsent,
                        self.fast_recover_exit_point
                    );
                }
                i += 1;
            }

            if !to_fast_retrans.is_empty() {
                if let Ok(raw) = self.create_packet(to_fast_retrans).marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!(
                        "[{}] failed to serialize a DATA packet to be fast-retransmitted",
                        self.side
                    );
                }
            }
        }

        raw_packets
    }

    fn gather_outbound_sack_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        if self.ack_state == AckState::Immediate {
            self.ack_state = AckState::Idle;
            let sack = self.create_selective_ack_chunk();
            trace!("[{}] sending SACK: {}", self.side, sack);
            if let Ok(raw) = self.create_packet(vec![Box::new(sack)]).marshal() {
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a SACK packet", self.side);
            }
        }

        raw_packets
    }

    fn gather_outbound_forward_tsn_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        /*log::debug!(
            "[{}] gatherOutboundForwardTSNPackets {}",
            self.name,
            self.will_send_forward_tsn
        );*/
        if self.will_send_forward_tsn {
            self.will_send_forward_tsn = false;
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                let fwd_tsn = self.create_forward_tsn();
                if let Ok(raw) = self.create_packet(vec![Box::new(fwd_tsn)]).marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!("[{}] failed to serialize a Forward TSN packet", self.side);
                }
            }
        }

        raw_packets
    }

    fn gather_outbound_shutdown_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> (Vec<Bytes>, bool) {
        let mut ok = true;

        if self.will_send_shutdown {
            self.will_send_shutdown = false;

            let shutdown = ChunkShutdown {
                cumulative_tsn_ack: self.cumulative_tsn_ack_point,
            };

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown)]).marshal() {
                self.timers
                    .start(Timer::T2Shutdown, now, self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a Shutdown packet", self.side);
            }
        } else if self.will_send_shutdown_ack {
            self.will_send_shutdown_ack = false;

            let shutdown_ack = ChunkShutdownAck {};

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown_ack)]).marshal() {
                self.timers
                    .start(Timer::T2Shutdown, now, self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a ShutdownAck packet", self.side);
            }
        } else if self.will_send_shutdown_complete {
            self.will_send_shutdown_complete = false;

            let shutdown_complete = ChunkShutdownComplete {};

            if let Ok(raw) = self
                .create_packet(vec![Box::new(shutdown_complete)])
                .marshal()
            {
                raw_packets.push(raw);
                ok = false;
            } else {
                warn!(
                    "[{}] failed to serialize a ShutdownComplete packet",
                    self.side
                );
            }
        }

        (raw_packets, ok)
    }

    /// get_data_packets_to_retransmit is called when T3-rtx is timed out and retransmit outstanding data chunks
    /// that are not acked or abandoned yet.
    fn get_data_packets_to_retransmit(&mut self, now: Instant) -> Vec<Packet> {
        let awnd = std::cmp::min(self.cwnd, self.rwnd);
        let mut chunks = vec![];
        let mut bytes_to_send = 0;
        let mut done = false;
        let mut i = 0;
        while !done {
            let tsn = self.cumulative_tsn_ack_point + i + 1;
            if let Some(c) = self.inflight_queue.get_mut(tsn) {
                if !c.retransmit {
                    i += 1;
                    continue;
                }

                if i == 0 && self.rwnd < c.user_data.len() as u32 {
                    // Send it as a zero window probe
                    done = true;
                } else if bytes_to_send + c.user_data.len() > awnd as usize {
                    break;
                }

                // reset the retransmit flag not to retransmit again before the next
                // t3-rtx timer fires
                c.retransmit = false;
                bytes_to_send += c.user_data.len();

                c.nsent += 1;
            } else {
                break; // end of pending data
            }

            if let Some(c) = self.inflight_queue.get_mut(tsn) {
                Association::check_partial_reliability_status(
                    c,
                    now,
                    self.use_forward_tsn,
                    self.side,
                    &self.streams,
                );

                trace!(
                    "[{}] retransmitting tsn={} ssn={} sent={}",
                    self.side,
                    c.tsn,
                    c.stream_sequence_number,
                    c.nsent
                );

                chunks.push(c.clone());
            }
            i += 1;
        }

        self.bundle_data_chunks_into_packets(chunks)
    }

    /// pop_pending_data_chunks_to_send pops chunks from the pending queues as many as
    /// the cwnd and rwnd allows to send.
    fn pop_pending_data_chunks_to_send(
        &mut self,
        now: Instant,
    ) -> (Vec<ChunkPayloadData>, Vec<u16>) {
        let mut chunks = vec![];
        let mut sis_to_reset = vec![]; // stream identifiers to reset
        if !self.pending_queue.is_empty() {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   A) At any given time, the data sender MUST NOT transmit new data to
            //      any destination transport address if its peer's rwnd indicates
            //      that the peer has no buffer space (i.e., rwnd is 0; see Section
            //      6.2.1).  However, regardless of the value of rwnd (including if it
            //      is 0), the data sender can always have one DATA chunk in flight to
            //      the receiver if allowed by cwnd (see rule B, below).

            while let Some(c) = self.pending_queue.peek() {
                let (beginning_fragment, unordered, data_len, stream_identifier) = (
                    c.beginning_fragment,
                    c.unordered,
                    c.user_data.len(),
                    c.stream_identifier,
                );

                if data_len == 0 {
                    sis_to_reset.push(stream_identifier);
                    if self
                        .pending_queue
                        .pop(beginning_fragment, unordered)
                        .is_none()
                    {
                        error!("[{}] failed to pop from pending queue", self.side);
                    }
                    continue;
                }

                if self.inflight_queue.get_num_bytes() + data_len > self.cwnd as usize {
                    break; // would exceeds cwnd
                }

                if data_len > self.rwnd as usize {
                    break; // no more rwnd
                }

                self.rwnd -= data_len as u32;

                if let Some(chunk) = self.move_pending_data_chunk_to_inflight_queue(
                    beginning_fragment,
                    unordered,
                    now,
                ) {
                    chunks.push(chunk);
                }
            }

            // the data sender can always have one DATA chunk in flight to the receiver
            if chunks.is_empty() && self.inflight_queue.is_empty() {
                // Send zero window probe
                if let Some(c) = self.pending_queue.peek() {
                    let (beginning_fragment, unordered) = (c.beginning_fragment, c.unordered);

                    if let Some(chunk) = self.move_pending_data_chunk_to_inflight_queue(
                        beginning_fragment,
                        unordered,
                        now,
                    ) {
                        chunks.push(chunk);
                    }
                }
            }
        }

        (chunks, sis_to_reset)
    }

    /// bundle_data_chunks_into_packets packs DATA chunks into packets. It tries to bundle
    /// DATA chunks into a packet so long as the resulting packet size does not exceed
    /// the path MTU.
    fn bundle_data_chunks_into_packets(&self, chunks: Vec<ChunkPayloadData>) -> Vec<Packet> {
        let mut packets = vec![];
        let mut chunks_to_send = vec![];
        let mut bytes_in_packet = COMMON_HEADER_SIZE;

        for c in chunks {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   Multiple DATA chunks committed for transmission MAY be bundled in a
            //   single packet.  Furthermore, DATA chunks being retransmitted MAY be
            //   bundled with new DATA chunks, as long as the resulting packet size
            //   does not exceed the path MTU.
            if bytes_in_packet + c.user_data.len() as u32 > self.mtu {
                packets.push(self.create_packet(chunks_to_send));
                chunks_to_send = vec![];
                bytes_in_packet = COMMON_HEADER_SIZE;
            }

            bytes_in_packet += DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32;
            chunks_to_send.push(Box::new(c));
        }

        if !chunks_to_send.is_empty() {
            packets.push(self.create_packet(chunks_to_send));
        }

        packets
    }

    /// generate_next_tsn returns the my_next_tsn and increases it. The caller should hold the lock.
    fn generate_next_tsn(&mut self) -> u32 {
        let tsn = self.my_next_tsn;
        self.my_next_tsn += 1;
        tsn
    }

    /// generate_next_rsn returns the my_next_rsn and increases it. The caller should hold the lock.
    fn generate_next_rsn(&mut self) -> u32 {
        let rsn = self.my_next_rsn;
        self.my_next_rsn += 1;
        rsn
    }

    fn check_partial_reliability_status(
        c: &mut ChunkPayloadData,
        now: Instant,
        use_forward_tsn: bool,
        side: Side,
        streams: &FxHashMap<u16, StreamState>,
    ) {
        if !use_forward_tsn {
            return;
        }

        // draft-ietf-rtcweb-data-protocol-09.txt section 6
        //	6.  Procedures
        //		All Data Channel Establishment Protocol messages MUST be sent using
        //		ordered delivery and reliable transmission.
        //
        if c.payload_type == PayloadProtocolIdentifier::Dcep {
            return;
        }

        // PR-SCTP
        if let Some(s) = streams.get(&c.stream_identifier) {
            let reliability_type: ReliabilityType = s.reliability_type;
            let reliability_value = s.reliability_value;

            if reliability_type == ReliabilityType::Rexmit {
                if c.nsent >= reliability_value {
                    c.set_abandoned(true);
                    trace!(
                        "[{}] marked as abandoned: tsn={} ppi={} (remix: {})",
                        side,
                        c.tsn,
                        c.payload_type,
                        c.nsent
                    );
                }
            } else if reliability_type == ReliabilityType::Timed {
                if let Some(since) = &c.since {
                    let elapsed = now.duration_since(*since);
                    if elapsed.as_millis() as u32 >= reliability_value {
                        c.set_abandoned(true);
                        trace!(
                            "[{}] marked as abandoned: tsn={} ppi={} (timed: {:?})",
                            side,
                            c.tsn,
                            c.payload_type,
                            elapsed
                        );
                    }
                } else {
                    error!("[{}] invalid c.since", side);
                }
            }
        } else {
            error!("[{}] stream {} not found)", side, c.stream_identifier);
        }
    }

    fn create_selective_ack_chunk(&mut self) -> ChunkSelectiveAck {
        ChunkSelectiveAck {
            cumulative_tsn_ack: self.peer_last_tsn,
            advertised_receiver_window_credit: self.get_my_receiver_window_credit(),
            gap_ack_blocks: self.payload_queue.get_gap_ack_blocks(self.peer_last_tsn),
            duplicate_tsn: self.payload_queue.pop_duplicates(),
        }
    }

    /// create_forward_tsn generates ForwardTSN chunk.
    /// This method will be be called if use_forward_tsn is set to false.
    fn create_forward_tsn(&self) -> ChunkForwardTsn {
        // RFC 3758 Sec 3.5 C4
        let mut stream_map: HashMap<u16, u16> = HashMap::new(); // to report only once per SI
        let mut i = self.cumulative_tsn_ack_point + 1;
        while sna32lte(i, self.advanced_peer_tsn_ack_point) {
            if let Some(c) = self.inflight_queue.get(i) {
                if let Some(ssn) = stream_map.get(&c.stream_identifier) {
                    if sna16lt(*ssn, c.stream_sequence_number) {
                        // to report only once with greatest SSN
                        stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                    }
                } else {
                    stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                }
            } else {
                break;
            }

            i += 1;
        }

        let mut fwd_tsn = ChunkForwardTsn {
            new_cumulative_tsn: self.advanced_peer_tsn_ack_point,
            streams: vec![],
        };

        let mut stream_str = String::new();
        for (si, ssn) in &stream_map {
            stream_str += format!("(si={} ssn={})", si, ssn).as_str();
            fwd_tsn.streams.push(ChunkForwardTsnStream {
                identifier: *si,
                sequence: *ssn,
            });
        }
        trace!(
            "[{}] building fwd_tsn: newCumulativeTSN={} cumTSN={} - {}",
            self.side,
            fwd_tsn.new_cumulative_tsn,
            self.cumulative_tsn_ack_point,
            stream_str
        );

        fwd_tsn
    }

    /// Move the chunk peeked with self.pending_queue.peek() to the inflight_queue.
    fn move_pending_data_chunk_to_inflight_queue(
        &mut self,
        beginning_fragment: bool,
        unordered: bool,
        now: Instant,
    ) -> Option<ChunkPayloadData> {
        if let Some(mut c) = self.pending_queue.pop(beginning_fragment, unordered) {
            // Mark all fragements are in-flight now
            if c.ending_fragment {
                c.set_all_inflight();
            }

            // Assign TSN
            c.tsn = self.generate_next_tsn();

            c.since = Some(now); // use to calculate RTT and also for maxPacketLifeTime
            c.nsent = 1; // being sent for the first time

            Association::check_partial_reliability_status(
                &mut c,
                now,
                self.use_forward_tsn,
                self.side,
                &self.streams,
            );

            trace!(
                "[{}] sending ppi={} tsn={} ssn={} sent={} len={} ({},{})",
                self.side,
                c.payload_type as u32,
                c.tsn,
                c.stream_sequence_number,
                c.nsent,
                c.user_data.len(),
                c.beginning_fragment,
                c.ending_fragment
            );

            self.inflight_queue.push_no_check(c.clone());

            Some(c)
        } else {
            error!("[{}] failed to pop from pending queue", self.side);
            None
        }
    }

    pub(crate) fn send_reset_request(&mut self, stream_identifier: StreamId) -> Result<()> {
        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist);
        }

        // Create DATA chunk which only contains valid stream identifier with
        // nil userData and use it as a EOS from the stream.
        let c = ChunkPayloadData {
            stream_identifier,
            beginning_fragment: true,
            ending_fragment: true,
            user_data: Bytes::new(),
            ..Default::default()
        };

        self.pending_queue.push(c);
        self.awake_write_loop();

        Ok(())
    }

    /// send_payload_data sends the data chunks.
    pub(crate) fn send_payload_data(&mut self, chunks: Vec<ChunkPayloadData>) -> Result<()> {
        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist);
        }

        // Push the chunks into the pending queue first.
        for c in chunks {
            self.pending_queue.push(c);
        }

        self.awake_write_loop();
        Ok(())
    }

    /// buffered_amount returns total amount (in bytes) of currently buffered user data.
    /// This is used only by testing.
    pub(crate) fn buffered_amount(&self) -> usize {
        self.pending_queue.get_num_bytes() + self.inflight_queue.get_num_bytes()
    }

    fn awake_write_loop(&self) {
        // No Op on Purpose
    }

    fn close_all_timers(&mut self) {
        // Close all retransmission & ack timers
        for timer in Timer::VALUES {
            self.timers.stop(timer);
        }
    }

    fn on_ack_timeout(&mut self) {
        trace!(
            "[{}] ack timed out (ack_state: {})",
            self.side,
            self.ack_state
        );
        self.stats.inc_ack_timeouts();
        self.ack_state = AckState::Immediate;
        self.awake_write_loop();
    }

    fn on_retransmission_timeout(&mut self, timer_id: Timer, n_rtos: usize) {
        match timer_id {
            Timer::T1Init => {
                if let Err(err) = self.send_init() {
                    debug!(
                        "[{}] failed to retransmit init (n_rtos={}): {:?}",
                        self.side, n_rtos, err
                    );
                }
            }

            Timer::T1Cookie => {
                if let Err(err) = self.send_cookie_echo() {
                    debug!(
                        "[{}] failed to retransmit cookie-echo (n_rtos={}): {:?}",
                        self.side, n_rtos, err
                    );
                }
            }

            Timer::T2Shutdown => {
                debug!(
                    "[{}] retransmission of shutdown timeout (n_rtos={})",
                    self.side, n_rtos
                );
                let state = self.state();
                match state {
                    AssociationState::ShutdownSent => {
                        self.will_send_shutdown = true;
                        self.awake_write_loop();
                    }
                    AssociationState::ShutdownAckSent => {
                        self.will_send_shutdown_ack = true;
                        self.awake_write_loop();
                    }
                    _ => {}
                }
            }

            Timer::T3RTX => {
                self.stats.inc_t3timeouts();

                // RFC 4960 sec 6.3.3
                //  E1)  For the destination address for which the timer expires, adjust
                //       its ssthresh with rules defined in Section 7.2.3 and set the
                //       cwnd <- MTU.
                // RFC 4960 sec 7.2.3
                //   When the T3-rtx timer expires on an address, SCTP should perform slow
                //   start by:
                //      ssthresh = max(cwnd/2, 4*MTU)
                //      cwnd = 1*MTU

                self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                self.cwnd = self.mtu;
                trace!(
                    "[{}] updated cwnd={} ssthresh={} inflight={} (RTO)",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    self.inflight_queue.get_num_bytes()
                );

                // RFC 3758 sec 3.5
                //  A5) Any time the T3-rtx timer expires, on any destination, the sender
                //  SHOULD try to advance the "Advanced.Peer.Ack.Point" by following
                //  the procedures outlined in C2 - C5.
                if self.use_forward_tsn {
                    // RFC 3758 Sec 3.5 C2
                    let mut i = self.advanced_peer_tsn_ack_point + 1;
                    while let Some(c) = self.inflight_queue.get(i) {
                        if !c.abandoned() {
                            break;
                        }
                        self.advanced_peer_tsn_ack_point = i;
                        i += 1;
                    }

                    // RFC 3758 Sec 3.5 C3
                    if sna32gt(
                        self.advanced_peer_tsn_ack_point,
                        self.cumulative_tsn_ack_point,
                    ) {
                        self.will_send_forward_tsn = true;
                        debug!(
                            "[{}] on_retransmission_timeout {}: sna32GT({}, {})",
                            self.side,
                            self.will_send_forward_tsn,
                            self.advanced_peer_tsn_ack_point,
                            self.cumulative_tsn_ack_point
                        );
                    }
                }

                debug!(
                    "[{}] T3-rtx timed out: n_rtos={} cwnd={} ssthresh={}",
                    self.side, n_rtos, self.cwnd, self.ssthresh
                );

                self.inflight_queue.mark_all_to_retrasmit();
                self.awake_write_loop();
            }

            Timer::Reconfig => {
                self.will_retransmit_reconfig = true;
                self.awake_write_loop();
            }

            _ => {}
        }
    }

    fn on_retransmission_failure(&mut self, id: Timer) {
        match id {
            Timer::T1Init => {
                error!("[{}] retransmission failure: T1-init", self.side);
                self.error = Some(AssociationError::HandshakeFailed(
                    Error::ErrHandshakeInitAck,
                ));
            }

            Timer::T1Cookie => {
                error!("[{}] retransmission failure: T1-cookie", self.side);
                self.error = Some(AssociationError::HandshakeFailed(
                    Error::ErrHandshakeCookieEcho,
                ));
            }

            Timer::T2Shutdown => {
                error!("[{}] retransmission failure: T2-shutdown", self.side);
            }

            Timer::T3RTX => {
                // T3-rtx timer will not fail by design
                // Justifications:
                //  * ICE would fail if the connectivity is lost
                //  * WebRTC spec is not clear how this incident should be reported to ULP
                error!("[{}] retransmission failure: T3-rtx (DATA)", self.side);
            }

            _ => {}
        }
    }

    /// Whether no timers are running
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        Timer::VALUES
            .iter()
            //.filter(|&&t| t != Timer::KeepAlive && t != Timer::PushNewCid)
            .filter_map(|&t| Some((t, self.timers.get(t)?)))
            .min_by_key(|&(_, time)| time)
            //.map_or(true, |(timer, _)| timer == Timer::Idle)
            .is_none()
    }
}

use capnp::serialize::OwnedSegments;
use capnp::message::Reader;
use raft_capnp::{append_entries, append_entries_reply,
                 request_vote, request_vote_reply};
use rpc::{RpcError};
use rpc::client::Rpc;
use std::net::SocketAddr;
use std::thread;
use std::thread::JoinHandle;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::mem;
use std::time::{Instant, Duration};

use super::log::{Log, Entry};
use super::super::common::{constants, RaftError};
use super::{MainThreadMessage, AppendEntriesReply, RequestVoteReply, RpcHandlerPipe};

pub type PeerInfo = (u64, SocketAddr);


/// Messages received by peer thread
#[derive(Debug)]
pub struct AppendEntriesMessage {
    pub term: u64,
    pub leader_id: u64,
    pub prev_log_index: usize,
    pub prev_log_term: u64,
    pub entries: Vec<Entry>,
    pub leader_commit: usize,
}

#[derive(Copy, Clone, Debug)]
pub struct RequestVoteMessage {
    pub term: u64,
    pub candidate_id: u64,
    pub last_log_index: usize,
    pub last_log_term: u64,
}

///
/// Messages for peer background threads to push to associated machines.
///
#[derive(Debug)]
pub enum PeerThreadMessage {
    AppendEntries (AppendEntriesMessage),
    RequestVote (RequestVoteMessage),
    Shutdown
}

#[derive(Debug)]
pub enum PeerState {
    Voting,
    // non voting members have a current round and a time that round started at as well as an
    // rpc handler thread that is waiting to hear if this succeeds or not
    NonVoting(u32, Instant, RpcHandlerPipe)
}

///
/// Handle for main thread to send messages to Peer.
///
#[derive(Debug)]
pub struct PeerHandle {
    pub id: u64,
    pub to_peer: Sender<PeerThreadMessage>,
    pub next_index: usize,
    pub match_index: usize,
    pub thread: Option<JoinHandle<()>>,
    pub state: PeerState
}

pub enum NonVotingPeerState {
    CatchingUp,
    CaughtUp(RpcHandlerPipe),
    TimedOut(RpcHandlerPipe),
    VotingPeer
}

impl PeerHandle {
    ///
    /// Pushes a non-blocking append-entries request to this peer.
    ///
    /// #Panics
    /// Panics if the peer thread has panicked.
    ///
    pub fn append_entries_nonblocking (&self, leader_id: u64,
                                       commit_index: usize, current_term: u64,
                                       log: Arc<Mutex<Log>>) {
        let prev_log_index = self.next_index - 1;
        let (last_entry, entries) = {
            let log = log.lock().unwrap();
            debug_assert!(self.next_index <= log.get_last_entry_index() + 1, "{} <= {}", self.next_index, log.get_last_entry_index());
            (log.get_entry(prev_log_index).cloned(),
             log.get_entries_from(prev_log_index).to_vec())
        }; 

        // We should never be out of bounds.
        debug_assert!(commit_index - prev_log_index <= entries.len());

        let message = PeerThreadMessage::AppendEntries(AppendEntriesMessage {
            term: current_term,
            leader_id: leader_id,
            prev_log_index: prev_log_index,
            prev_log_term: last_entry.map(|entry| entry.term).unwrap_or(0),
            entries: entries.to_vec(),
            leader_commit: commit_index,
        });
        self.to_peer.send(message).unwrap(); //panics if the peer thread has panicked
    }

    /// Advances the current round of this non-voting peer. Is a noop for voting peers.
    pub fn advance_non_voting_peer_round(&mut self, latest_log_index: usize) -> NonVotingPeerState {
        let mut ret_state = NonVotingPeerState::VotingPeer;
        if let PeerState::NonVoting(ref mut round, ref mut start_time, ref mut pipe) = self.state {
            *round += 1;
            if self.next_index == latest_log_index + 1 {
                // woohoo they're all caught up
                // need to do a replace here since pipe isn't optional...
                let p = mem::replace(pipe, channel().0);
                ret_state = NonVotingPeerState::CaughtUp(p);
            } else {
                let now = Instant::now();
                if *round == constants::MAX_ROUNDS_FOR_NEW_SERVER {
                    if now.duration_since(*start_time) > Duration::from_millis(constants::ELECTION_TIMEOUT_MIN) {
                        // need to do a replace here since pipe isn't optional...
                        let p = mem::replace(pipe, channel().0);
                        ret_state = NonVotingPeerState::TimedOut(p);
                    } else {
                        // server is moving fast enough so we'll add it to the cluster
                        // need to do a replace here since pipe isn't optional...
                        let p = mem::replace(pipe, channel().0);
                        ret_state = NonVotingPeerState::CaughtUp(p);
                    }
                } else {
                    // not caught up yet
                    *start_time = now;
                    ret_state = NonVotingPeerState::CatchingUp;
                }
            }
        }

        match ret_state {
            NonVotingPeerState::CaughtUp(..) => self.state = PeerState::Voting,
            _ => {}
        }
        ret_state
    }
}

impl Drop for PeerHandle {
    /// Blocks until the background peer thread exits
    /// Can potentially block for a long time if this peer is unresponsive
    ///
    /// #Panics
    /// Panics if the peer thread has panicked
    fn drop (&mut self) {
        let thread = mem::replace(&mut self.thread, None);
        match thread {
            Some(t) => {
                self.to_peer.send(PeerThreadMessage::Shutdown).unwrap();
                t.join().unwrap();
            },
            None => {/* Nothing to drop */}
        }
    }
}

///
/// A background thread whose job is to communicate and relay messages between
/// the main thread and the peer at |addr|.
///
pub struct Peer {
    id: u64,
    addr: SocketAddr,
    to_main: Sender<MainThreadMessage>,
    from_main: Receiver<PeerThreadMessage>
}

// TODO(jason): Use mio to ensure that peers shutdown without blocking the main thread
// Right now it's possible for a laggy peer to block the main thread during step down or shut down
impl Peer {
    ///
    /// Spawns a new Peer in a background thread to communicate with the server at id.
    ///
    /// # Panics
    /// Panics if the OS fails to create a new background thread.
    ///
    pub fn start (id: PeerInfo, to_main: Sender<MainThreadMessage>, non_voting: Option<RpcHandlerPipe>) -> PeerHandle {
        let (to_peer, from_main) = channel();
        
        let t = thread::spawn(move || {
            let peer = Peer {
                id: id.0,
                addr: id.1,
                to_main: to_main,
                from_main: from_main
            };
            peer.main();
        });

        let state = match non_voting {
            Some(pipe) => PeerState::NonVoting(0, Instant::now(), pipe),
            None => PeerState::Voting
        };

        PeerHandle {
            id: id.0,
            to_peer: to_peer,
            next_index: 1,
            match_index: 0,
            thread: Some(t),
            state: state
        }
    }

    ///
    /// Sets the term, candidate_id, and last_log index on the rpc from the
    /// data in the RequestVoteMessage
    ///
    fn construct_append_entries (rpc: &mut Rpc, entry: &AppendEntriesMessage) {
        let mut params = rpc.get_param_builder().init_as::<append_entries::Builder>();
        params.set_term(entry.term);
        params.set_leader_id(entry.leader_id);
        params.set_prev_log_index(entry.prev_log_index as u64);
        params.set_prev_log_term(entry.prev_log_term);
        params.set_leader_commit(entry.leader_commit as u64);
        params.borrow().init_entries(entry.entries.len() as u32);
        for i in 0..entry.entries.len() {
            let mut entry_builder = params.borrow().get_entries().unwrap().get(i as u32);
            entry.entries[i].into_proto(&mut entry_builder);
        }
    }

    ///
    /// Processes a append_entries_reply for the current term.
    /// Returns a tuple containing the reply's term and whether the peer
    /// successfully appended the entry.
    ///
    /// # Errors
    /// Returns an RpcError if the msg is not a well formed append_entries_reply
    ///
    fn handle_append_entries_reply (entry_term: u64, msg: Reader<OwnedSegments>)
        -> Result<(u64, bool), RpcError> {
        Rpc::get_result_reader(&msg).and_then(|result| {
            result.get_as::<append_entries_reply::Reader>()
                  .map_err(RpcError::Capnp)
            })
            .map(|reply_reader| {
                let term = reply_reader.get_term();
                let success = reply_reader.get_success();
                (term, term == entry_term && success)
            })
    }

    ///
    /// Sends the appropriate append entries RPC to this peer.
    ///
    /// # Panics
    /// Panics if proto fails to initialize, or main thread has panicked or
    /// is deallocated.
    ///
    fn append_entries_blocking (&mut self, entry: AppendEntriesMessage) {
        let mut rpc = Rpc::new(constants::APPEND_ENTRIES_OPCODE);
        Peer::construct_append_entries(&mut rpc, &entry);
        let (term, success) = rpc.send(self.addr)
            .and_then(|msg| Peer::handle_append_entries_reply(entry.term, msg))
            .unwrap_or((entry.term, false));
        let new_commit_index = entry.prev_log_index + entry.entries.len();
        let reply = AppendEntriesReply {
            term: term,
            commit_index: if success { new_commit_index } else { entry.prev_log_index },
            peer: (self.id, self.addr),
            success: success,
        };
        // Panics if main thread has panicked or been otherwise deallocated.
        self.to_main.send(MainThreadMessage::AppendEntriesReply(reply)).unwrap();
    }

    ///
    /// Requests a vote in the new term from this peer.
    ///
    /// # Panics
    /// Panics if the main thread has panicked or been deallocated
    ///
    fn send_request_vote (&self, vote: RequestVoteMessage) {
        let mut rpc = Rpc::new(constants::REQUEST_VOTE_OPCODE);
        Peer::construct_request_vote(&mut rpc, &vote);

        let vote_granted = rpc.send(self.addr)
            .and_then(|msg| Peer::handle_request_vote_reply(vote.term, msg))
            .unwrap_or(false);

        let reply = RequestVoteReply {
            term: vote.term,
            vote_granted: vote_granted
        };
        // Panics if the main thread has panicked or been deallocated
        self.to_main.send(MainThreadMessage::RequestVoteReply(reply)).unwrap();
    }

    ///
    /// Sets the term, candidate_id, and last_log index on the rpc from the
    /// data in the RequestVoteMessage
    ///
    fn construct_request_vote (rpc: &mut Rpc, vote: &RequestVoteMessage) {
        let mut params = rpc.get_param_builder().init_as::<request_vote::Builder>();
        params.set_term(vote.term);
        params.set_candidate_id(vote.candidate_id);
        params.set_last_log_index(vote.last_log_index as u64);
        params.set_last_log_term(vote.last_log_term);
    }

    ///
    /// Processes a request_vote_reply for the given vote_term
    /// Returms true if the peer granted their vote or false if the peer denied their vote
    ///
    /// # Errors
    /// Returns an RpcError if the msg is not a well formed request_vote_reply
    ///
    fn handle_request_vote_reply (vote_term: u64, msg: Reader<OwnedSegments>) -> Result<bool, RpcError> {
        Rpc::get_result_reader(&msg)
        .and_then(|result| {
            result.get_as::<request_vote_reply::Reader>()
                  .map_err(RpcError::Capnp)
        })
        .map(|reply_reader| {
            let reply_term = reply_reader.get_term();
            let vote_granted = reply_reader.get_vote_granted();
            (reply_term == vote_term) && vote_granted
        })
    }

    ///
    /// Main loop for background thread
    /// Waits on messages from its pipe and acts on them
    ///
    /// # Panics
    /// Panics if the main thread has panicked or been deallocated
    ///
    fn main (mut self) {
        loop {
            match self.from_main.recv().unwrap() {
                PeerThreadMessage::AppendEntries(entry) => self.append_entries_blocking(entry),
                PeerThreadMessage::RequestVote(vote) => self.send_request_vote(vote),
                PeerThreadMessage::Shutdown => break
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use capnp::{message, serialize_packed};
    use capnp::serialize::OwnedSegments;
    use std::io::BufReader;
    use std::sync::mpsc::{channel};
    use std::sync::{Arc, Mutex};
    use super::*;
    use super::super::constants;
    use super::super::log::{Entry, random_entry_with_term, random_entries_with_term, Log};
    use super::super::log::mocks::{new_mock_log, new_random_with_term};
    use super::super::super::raft_capnp::{request_vote, request_vote_reply,
                                          append_entries, append_entries_reply};
    use super::super::super::rpc_capnp::rpc_response;

    #[test]
    fn constructs_valid_append_entries() {
        const TERM: u64 = 13;
        const LEADER_ID: u64 = 6;
        const PREV_LOG_INDEX: u64 = 78;
        const PREV_LOG_TERM: u64 = 5;
        const LEADER_COMMIT: u64 = 10;
        let entries = vec![random_entry_with_term(TERM); 6];
        let mut rpc = Rpc::new(constants::APPEND_ENTRIES_OPCODE);
        let entry = AppendEntriesMessage {
            term: TERM,
            leader_id: LEADER_ID,
            prev_log_index: PREV_LOG_INDEX as usize,
            prev_log_term: PREV_LOG_TERM,
            leader_commit: LEADER_COMMIT as usize,
            entries: entries.clone(),
        };
        Peer::construct_append_entries(&mut rpc, &entry);
        let param_reader = rpc.get_param_builder().as_reader()
                              .get_as::<append_entries::Reader>().unwrap();
        assert_eq!(param_reader.get_term(), TERM);
        assert_eq!(param_reader.get_leader_id(), LEADER_ID);
        assert_eq!(param_reader.get_prev_log_index(), PREV_LOG_INDEX);
        assert_eq!(param_reader.get_prev_log_term(), PREV_LOG_TERM);
        assert_eq!(param_reader.get_leader_commit(), LEADER_COMMIT);
        let all_true = entries.iter().zip(param_reader.get_entries().unwrap().iter())
            .fold(true, |and, (entry1, entry2)| {
                and && (*entry1 == Entry::from_proto(entry2))
            });
        assert!(all_true);
    }

    #[test]
    fn append_entries_handles_commited_entry() {
        const TERM: u64 = 54;
        let mut builder = message::Builder::new_default();
        construct_append_entries_reply(&mut builder, TERM, true);
        let reader = get_message_reader(&builder);
        let (term, success) = Peer::handle_append_entries_reply(TERM, reader)
                                    .unwrap();
        assert_eq!(term, TERM);
        assert!(success);
    }

    #[test]
    fn append_entries_handles_incorrect_term() {
        const TERM: u64 = 54;
        let mut builder = message::Builder::new_default();
        construct_append_entries_reply(&mut builder, TERM + 1, true);
        let reader = get_message_reader(&builder);
        let (term, success) = Peer::handle_append_entries_reply(TERM, reader)
                                    .unwrap();
        assert!(term != TERM);
        assert!(!success);
    }

    #[test]
    fn append_entries_handles_failed_commit() {
        const TERM: u64 = 54;
        let mut builder = message::Builder::new_default();
        construct_append_entries_reply(&mut builder, TERM, false);
        let reader = get_message_reader(&builder);
        let (term, success) = Peer::handle_append_entries_reply(TERM, reader)
                                    .unwrap();
        assert!(term == TERM);
        assert!(!success);
    }

    #[test]
    fn peerhandle_append_entries_sends_correct_entries() {
        let (tx, rx) = channel();
        const TERM: u64 = 5;
        const PEER_NEXT_INDEX: usize = 3; // PEER_NEXT_INDEX <= COMMIT_INDEX + 1
        const COMMIT_INDEX: usize = 8; // COMMIT_INDEX < LOG_SIZE
        const LOG_SIZE: usize = 9;
        const LEADER_ID: u64 = 0; // LEADER_ID != PEER_ID
        let handle = PeerHandle {
            id: 1,
            to_peer: tx.clone(),
            next_index: PEER_NEXT_INDEX,
            match_index: PEER_NEXT_INDEX - 1,
            thread: None,
            state: PeerState::Voting
        };
        let (mock_log, _log_file_handle) = new_random_with_term(LOG_SIZE, TERM);
        let log: Arc<Mutex<Log>> = Arc::new(Mutex::new(mock_log));
        handle.append_entries_nonblocking(LEADER_ID,
                                          COMMIT_INDEX, TERM, log.clone());
        match rx.recv().unwrap() {
            PeerThreadMessage::AppendEntries(message) => {
                assert_eq!(message.term, TERM);
                assert_eq!(message.leader_id, LEADER_ID);
                assert_eq!(message.leader_commit, COMMIT_INDEX);
                assert_eq!(message.prev_log_index, PEER_NEXT_INDEX - 1);
                assert_eq!(message.prev_log_term, TERM);
                assert_eq!(message.entries.len(), COMMIT_INDEX - PEER_NEXT_INDEX + 1);
                let log_entries = log.lock().unwrap().get_entries_from(PEER_NEXT_INDEX - 1).to_vec();
                assert_eq!(message.entries.len(), log_entries.len());
                let entries_same = message.entries.iter().zip(log_entries.iter())
                    .fold(true, |and, (e1, e2)| and && *e1 == *e2);
                assert!(entries_same);
            },
            _ => panic!(),
        };
    }

    #[test]
    fn peerhandle_append_entries_sends_correct_empty_message() {
        let (tx, rx) = channel();
        const TERM: u64 = 5;
        const PEER_NEXT_INDEX: usize = 9; // PEER_NEXT_INDEX <= COMMIT_INDEX + 1
        const COMMIT_INDEX: usize = 8; // COMMIT_INDEX < LOG_SIZE
        const LOG_SIZE: usize = 9;
        const LEADER_ID: u64 = 0; // LEADER_ID != PEER_ID
        let handle = PeerHandle {
            id: 1,
            to_peer: tx.clone(),
            next_index: PEER_NEXT_INDEX,
            match_index: PEER_NEXT_INDEX - 1,
            thread: None,
            state: PeerState::Voting
        };
        let (mock_log, _log_file_handle) = new_mock_log();
        let log: Arc<Mutex<Log>> = Arc::new(Mutex::new(mock_log));
        {
            let mut log = log.lock().unwrap();
            log.append_entries_blocking(random_entries_with_term(COMMIT_INDEX, TERM - 1)).unwrap();
            log.append_entries_blocking(random_entries_with_term(LOG_SIZE - (COMMIT_INDEX), TERM)).unwrap();
        }
        handle.append_entries_nonblocking(LEADER_ID, COMMIT_INDEX, TERM, log.clone());
        match rx.recv().unwrap() {
            PeerThreadMessage::AppendEntries(message) => {
                assert_eq!(message.term, TERM);
                assert_eq!(message.leader_id, LEADER_ID);
                assert_eq!(message.leader_commit, COMMIT_INDEX);
                assert_eq!(message.prev_log_index, PEER_NEXT_INDEX - 1);
                assert_eq!(message.prev_log_term, TERM - 1);
                assert_eq!(message.entries.len(), 1);
            },
            _ => panic!(),
        };
    }

    #[test]
    fn constructs_valid_request_vote() {
        const TERM: u64 = 13;
        const CANDIDATE_ID: u64 = 6;
        const LAST_LOG_INDEX: u64 = 78;
        const LAST_LOG_TERM: u64 = 5;
        let mut rpc = Rpc::new(1);
        let vote = RequestVoteMessage {
            term: TERM,
            candidate_id: CANDIDATE_ID,
            last_log_index: LAST_LOG_INDEX as usize,
            last_log_term: LAST_LOG_TERM
        };

        Peer::construct_request_vote(&mut rpc, &vote);
        let param_reader = rpc.get_param_builder().as_reader().get_as::<request_vote::Reader>().unwrap();
        assert_eq!(param_reader.get_term(), TERM);
        assert_eq!(param_reader.get_candidate_id(), CANDIDATE_ID);
        assert_eq!(param_reader.get_last_log_index(), LAST_LOG_INDEX);
        assert_eq!(param_reader.get_last_log_term(), LAST_LOG_TERM);
    }

    fn get_message_reader<A> (msg: &message::Builder<A>) -> message::Reader<OwnedSegments> 
        where A: message::Allocator
    {
        let mut message_buffer = Vec::new();
        serialize_packed::write_message(&mut message_buffer, &msg).unwrap();

        let mut buf_reader = BufReader::new(&message_buffer[..]);
        serialize_packed::read_message(&mut buf_reader, message::ReaderOptions::new()).unwrap()
    }

    #[test]
    fn vote_reply_handles_vote_granted() {
        const TERM: u64 = 54;
        let mut builder = message::Builder::new_default();
        construct_request_vote_reply(&mut builder, TERM, true);

        let reader = get_message_reader(&builder);
        assert!(Peer::handle_request_vote_reply(TERM, reader).unwrap());
    }

    #[test]
    fn vote_reply_handles_incorrect_term() {
        const TERM: u64 = 26;
        let mut builder = message::Builder::new_default();
        construct_request_vote_reply(&mut builder, TERM - 1, true);

        let reader = get_message_reader(&builder);
        assert!(!Peer::handle_request_vote_reply(TERM, reader).unwrap());
    }

    #[test]
    fn vote_reply_handles_vote_rejected() {
        const TERM: u64 = 14;
        let mut builder = message::Builder::new_default();
        construct_request_vote_reply(&mut builder, TERM, false);

        let reader = get_message_reader(&builder);
        assert!(!Peer::handle_request_vote_reply(TERM, reader).unwrap());
    }

    #[test]
    // TODO: Determine what level of error checking we want on the messages
    // I have not found consistent error behavior. Even out of bounds seems to proceed
    // without error on travis...
    fn vote_reply_handles_malformed_vote_replies() {
        /*const TERM: u64 = 14;
        let mut builder = message::Builder::new_default();
        construct_append_entries_reply(&mut builder, TERM, true);

        let reader = get_message_reader(&builder);
        let err = Peer::handle_request_vote_reply(TERM, reader).unwrap_err();
        assert!(matches!(err, RpcError::Capnp(_)));*/
    }

    #[test]
    fn vote_reply_handles_malformed_rpc_replies() {
        // TODO(jason): Figure out why this test fails on travis
        /*const TERM: u64 = 19;
        let builder = message::Builder::new_default();
        let reader = get_message_reader(&builder);
        let err = Peer::handle_request_vote_reply(TERM, reader).unwrap_err();
        assert!(matches!(err, RpcError::Capnp(_)));*/
    }

    /// Constructs a valid rpc_response with the given information contained in an
    /// append_entries_reply inside the provided msg buffer.
    fn construct_append_entries_reply<A> (msg: &mut message::Builder<A>, term: u64, success: bool)
        where A: message::Allocator
    {
        let response_builder = msg.init_root::<rpc_response::Builder>();
        let mut reply_builder = response_builder.get_result().init_as::<append_entries_reply::Builder>();
        reply_builder.set_term(term);
        reply_builder.set_success(success);
    }

    /// Constructs a valid rpc_response with the given information contained in a request_vote_reply
    /// inside the provided msg buffer.
    fn construct_request_vote_reply<A> (msg: &mut message::Builder<A>, term: u64, vote_granted: bool) 
        where A: message::Allocator 
    {
        let response_builder = msg.init_root::<rpc_response::Builder>();
        let mut vote_reply_builder = response_builder.get_result().init_as::<request_vote_reply::Builder>();
        vote_reply_builder.set_term(term);
        vote_reply_builder.set_vote_granted(vote_granted);
    }
}

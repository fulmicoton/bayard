use std::{fs, thread};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

use crossbeam_channel::select;
use async_std::task::block_on;
use futures::Future;
use grpcio::{ChannelBuilder, EnvBuilder, Environment, RpcContext, ServerBuilder, UnarySink};
use log::*;
use protobuf::Message;
use raft::eraftpb::{ConfChange, ConfChangeType, Entry, EntryType, Message as RaftMessage};
use tantivy::{Document, Index, IndexWriter, Term};
use tantivy::collector::{Count, FacetCollector, MultiCollector, TopDocs};
use tantivy::query::{QueryParser, TermQuery};
use tantivy::schema::{Field, FieldType, IndexRecordOption, Schema};

use crate::client::client::{Clerk, create_client};
use crate::proto::indexpb_grpc::{self, Index as IndexService, IndexClient};
use crate::proto::indexrpcpb::{
    ApplyReq, CommitResp, ConfChangeReq, DeleteResp, GetReq, GetResp, JoinReq, LeaveReq, MergeResp,
    MetricsReq, MetricsResp, PeersReq, PeersResp, ProbeReq, ProbeResp, PutResp, RaftDone, ReqType,
    RespErr, RollbackResp, SchemaReq, SchemaResp, SearchReq, SearchResp,
};
use crate::server::{peer, util};
use crate::server::metrics::Metrics;
use crate::server::peer::PeerMessage;
use crate::util::search_result::{ScoredNamedFieldDocument, SearchResult};
use crate::util::signal::sigterm_channel;

struct NotifyArgs(u64, String, RespErr);

#[derive(Clone)]
pub struct IndexServer {
    id: u64,
    peers: Arc<Mutex<HashMap<u64, IndexClient>>>,
    peers_addr: Arc<Mutex<HashMap<u64, String>>>,
    rf_message_ch: SyncSender<PeerMessage>,
    notify_ch_map: Arc<Mutex<HashMap<u64, SyncSender<NotifyArgs>>>>,
    index: Arc<Index>,
    unique_key_field_name: String,
    index_writer: Arc<Mutex<IndexWriter>>,
    metrics: Arc<Mutex<Metrics>>,
}

impl IndexServer {
    pub fn start_server(
        id: u64,
        host: &str,
        port: u16,
        peers_addr: HashMap<u64, String>,
        data_directory: &str,
        schema_file: &str,
        unique_key_field_name: &str,
    ) {
        let mut peers = HashMap::new();
        peers.insert(id, create_client(&format!("{}:{}", host, port)));
        for (peer_id, peer_addr) in peers_addr.iter() {
            peers.insert(*peer_id, create_client(peer_addr));
        }

        let raft_path = Path::new(data_directory).join(Path::new("raft"));
        fs::create_dir_all(&raft_path).unwrap_or_default();

        let index_path = Path::new(data_directory).join(Path::new("index"));
        let index = if index_path.exists() {
            Index::open_in_dir(index_path.to_str().unwrap()).unwrap()
        } else {
            let schema_content = fs::read_to_string(schema_file).unwrap();
            let schema: Schema =
                serde_json::from_str(&schema_content).expect("error while reading json");
            fs::create_dir_all(&index_path).unwrap_or_default();
            Index::create_in_dir(index_path.to_str().unwrap(), schema).unwrap()
        };

        let num_threads = 1;
        let buffer_size_per_thread = 50_000_000;
        let index_writer = if num_threads > 0 {
            index
                .writer_with_num_threads(num_threads, buffer_size_per_thread)
                .unwrap()
        } else {
            index.writer(buffer_size_per_thread).unwrap()
        };

        let (rf_sender, rf_receiver) = mpsc::sync_channel(100);
        let (rpc_sender, rpc_receiver) = mpsc::sync_channel(100);
        let (apply_sender, apply_receiver) = mpsc::sync_channel(100);

        let peers_id = peers.keys().map(|id| *id).collect();

        let mut index_server = IndexServer {
            id,
            peers: Arc::new(Mutex::new(peers)),
            peers_addr: Arc::new(Mutex::new(peers_addr)),
            rf_message_ch: rf_sender,
            notify_ch_map: Arc::new(Mutex::new(HashMap::new())),
            index: Arc::new(index),
            unique_key_field_name: unique_key_field_name.to_string(),
            index_writer: Arc::new(Mutex::new(index_writer)),
            metrics: Arc::new(Mutex::new(Metrics::new(id))),
        };

        index_server.async_rpc_sender(rpc_receiver);
        index_server.async_applier(apply_receiver);

        let env = Arc::new(Environment::new(10));
        let service = indexpb_grpc::create_index(index_server.clone());
        let mut server = ServerBuilder::new(env)
            .register_service(service)
            .bind(host, port)
            .build()
            .unwrap_or_else(|e| {
                panic!("build server error: {}", e);
            });

        server.start();
        for &(ref host, port) in server.bind_addrs() {
            info!("listening on {}:{}", host, port);
        }

        let peer = peer::Peer::new(id, apply_sender, peers_id);
        peer::Peer::activate(peer, rpc_sender, rf_receiver);

        let mut servers: Vec<IndexClient> = Vec::new();
        for (_, value) in index_server.peers.clone().lock().unwrap().iter() {
            servers.push(value.clone());
        }

        let client_id = rand::random();
        let mut client = Clerk::new(&servers, client_id);
        client.join(id, host, port);

        // Wait for signals for termination (SIGINT, SIGTERM).
        let sigterm_receiver = sigterm_channel().unwrap();
        loop {
            select! {
                recv(sigterm_receiver) -> _ => {
                    info!("stopping on {}:{}", host, port);
                    let _ = server.shutdown().wait();
                    break;
                }
            }
        }
    }

    fn async_rpc_sender(&mut self, receiver: Receiver<RaftMessage>) {
        let l = self.peers.clone();
        thread::spawn(move || loop {
            match receiver.recv() {
                Ok(m) => {
                    let peers = l.lock().unwrap();
                    let op = peers.get(&m.to);
                    if let Some(c) = op {
                        let client = c.clone();
                        thread::spawn(move || {
                            client.raft(&m).unwrap_or_else(|e| {
                                error!("send raft msg to {} failed: {:?}", m.to, e);
                                RaftDone::new()
                            });
                        });
                    }
                }
                Err(_) => (),
            }
        });
    }

    fn start_op(&mut self, req: &ApplyReq) -> (RespErr, String) {
        let (sh, rh) = mpsc::sync_channel(0);
        {
            let mut map = self.notify_ch_map.lock().unwrap();
            map.insert(req.get_client_id(), sh);
        }
        self.rf_message_ch
            .send(PeerMessage::Propose(req.write_to_bytes().unwrap_or_else(
                |e| {
                    panic!("request write to bytes error: {}", e);
                },
            )))
            .unwrap_or_else(|e| {
                error!("send propose to raft error: {}", e);
            });
        match rh.recv_timeout(Duration::from_millis(1000)) {
            Ok(args) => {
                return (args.2, args.1);
            }
            Err(_) => {
                {
                    let mut map = self.notify_ch_map.lock().unwrap();
                    map.remove(&req.get_client_id());
                }
                return (RespErr::ErrWrongLeader, String::from(""));
            }
        }
    }

    // TODO: check duplicate request.
    fn async_applier(&mut self, apply_receiver: Receiver<Entry>) {
        let notify_ch_map = self.notify_ch_map.clone();
        let peers = self.peers.clone();
        let peers_addr = self.peers_addr.clone();
        let index = self.index.clone();
        let unique_key_field_name = self.unique_key_field_name.clone();
        let index_writer = self.index_writer.clone();
        let metrics = self.metrics.clone();

        thread::spawn(move || loop {
            match apply_receiver.recv() {
                Ok(e) => match e.get_entry_type() {
                    EntryType::EntryNormal => {
                        let result: NotifyArgs;
                        let req: ApplyReq = util::parse_data(e.get_data());
                        let client_id = req.get_client_id();
                        if e.data.len() > 0 {
                            result = Self::apply_entry(
                                e.term,
                                &req,
                                peers.clone(),
                                peers_addr.clone(),
                                index.clone(),
                                unique_key_field_name.as_str(),
                                index_writer.clone(),
                                metrics.clone(),
                            );
                            debug!("{:?}: {:?}", result.2, req);
                        } else {
                            result = NotifyArgs(0, String::from(""), RespErr::ErrWrongLeader);
                            debug!("{:?}", req);
                        }
                        let mut map = notify_ch_map.lock().unwrap();
                        if let Some(s) = map.get(&client_id) {
                            s.send(result).unwrap_or_else(|e| {
                                error!("notify apply result error: {}", e);
                            });
                        }
                        map.remove(&client_id);
                    }
                    EntryType::EntryConfChange => {
                        let result = NotifyArgs(0, String::from(""), RespErr::OK);
                        let cc: ConfChange = util::parse_data(e.get_data());
                        let mut map = notify_ch_map.lock().unwrap();
                        if let Some(s) = map.get(&cc.get_node_id()) {
                            s.send(result).unwrap_or_else(|e| {
                                error!("notify apply result error: {}", e);
                            });
                        }
                        map.remove(&cc.get_node_id());
                    }
                },
                Err(_) => (),
            }
        });
    }

    fn apply_entry(
        term: u64,
        req: &ApplyReq,
        peers: Arc<Mutex<HashMap<u64, IndexClient>>>,
        peers_addr: Arc<Mutex<HashMap<u64, String>>>,
        index: Arc<Index>,
        unique_key_field_name: &str,
        index_writer: Arc<Mutex<IndexWriter>>,
        metrics: Arc<Mutex<Metrics>>,
    ) -> NotifyArgs {
        debug!("{:?}", &req);
        match req.req_type {
            ReqType::Join => {
                metrics.lock().unwrap().inc_request_count("join");

                let mut prs = peers.lock().unwrap();
                let env = Arc::new(EnvBuilder::new().build());
                let ch = ChannelBuilder::new(env).connect(&req.get_join_req().peer_addr);
                prs.insert(req.get_join_req().peer_id, IndexClient::new(ch));

                let mut prs_addr = peers_addr.lock().unwrap();
                prs_addr.insert(
                    req.get_join_req().peer_id,
                    req.get_join_req().peer_addr.clone(),
                );

                NotifyArgs(term, String::from(""), RespErr::OK)
            }
            ReqType::Leave => {
                metrics.lock().unwrap().inc_request_count("leave");

                let mut prs = peers.lock().unwrap();
                prs.remove(&req.get_leave_req().peer_id);

                let mut prs_addr = peers_addr.lock().unwrap();
                prs_addr.remove(&req.get_leave_req().peer_id);

                NotifyArgs(term, String::from(""), RespErr::OK)
            }
            ReqType::Put => {
                metrics.lock().unwrap().inc_request_count("put");

                let mut doc = index
                    .schema()
                    .parse_document(req.get_put_req().get_fields())
                    .unwrap();
                let field = index.schema().get_field(unique_key_field_name).unwrap();
                doc.add_text(field, req.get_put_req().get_doc_id());
                index_writer
                    .lock()
                    .unwrap()
                    .delete_term(Term::from_field_text(field, req.get_put_req().get_doc_id()));
                let opstamp = index_writer.lock().unwrap().add_document(doc);

                let mut ret = HashMap::new();
                ret.insert("opstamp", opstamp);

                NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK)
            }
            ReqType::Delete => {
                metrics.lock().unwrap().inc_request_count("delete");

                let opstamp = index_writer
                    .lock()
                    .unwrap()
                    .delete_term(Term::from_field_text(
                        index.schema().get_field(unique_key_field_name).unwrap(),
                        req.get_delete_req().get_doc_id(),
                    ));

                let mut ret = HashMap::new();
                ret.insert("opstamp", opstamp);

                NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK)
            }
            ReqType::Commit => {
                metrics.lock().unwrap().inc_request_count("commit");

                match index_writer.lock().unwrap().commit() {
                    Ok(opstamp) => {
                        info!("commit succeeded");

                        let mut ret = HashMap::new();
                        ret.insert("opstamp", opstamp);

                        NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK)
                    }
                    Err(e) => {
                        error!("commit failed: {:?}", e);

                        // TODO: rollback index

                        let mut ret = HashMap::new();
                        ret.insert("error", format!("{:?}", e));

                        NotifyArgs(
                            term,
                            serde_json::to_string(&ret).unwrap(),
                            RespErr::ErrWrongLeader,
                        )
                    }
                }
            }
            ReqType::Rollback => {
                metrics.lock().unwrap().inc_request_count("rollback");

                match index_writer.lock().unwrap().rollback() {
                    Ok(opstamp) => {
                        info!("rollback succeed");

                        let mut ret = HashMap::new();
                        ret.insert("opstamp", opstamp);

                        NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK)
                    }
                    Err(e) => {
                        error!("rollback failed: {:?}", e);

                        let mut ret = HashMap::new();
                        ret.insert("error", format!("{:?}", e));

                        NotifyArgs(
                            term,
                            serde_json::to_string(&ret).unwrap(),
                            RespErr::ErrWrongLeader,
                        )
                    }
                }
            }
            ReqType::Merge => {
                metrics.lock().unwrap().inc_request_count("merge");

                let segments = index.searchable_segment_ids().unwrap();

                // check segments length
                if segments.len() <= 0 {
                    // do not merge segments
                    let mut ret = HashMap::new();
                    ret.insert("segments", segments);

                    return NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK);
                }

                let merge_future = index_writer.lock().unwrap().merge(&segments);
                match block_on(merge_future)
                {
                    Ok(segment_meta) => {
                        info!("merge succeed: {:?}", segment_meta);

                        let mut ret = HashMap::new();
                        ret.insert("segment_meta", segment_meta);
                        NotifyArgs(term, serde_json::to_string(&ret).unwrap(), RespErr::OK)
                    }
                    Err(e) => {
                        error!("merge failed: {:?}", e);

                        let mut ret = HashMap::new();
                        ret.insert("error", format!("{:?}", e));

                        NotifyArgs(
                            term,
                            serde_json::to_string(&ret).unwrap(),
                            RespErr::ErrWrongLeader,
                        )
                    }
                }
            }
        }
    }
}

impl IndexService for IndexServer {
    fn raft(&mut self, ctx: RpcContext, req: RaftMessage, sink: UnarySink<RaftDone>) {
        self.metrics.lock().unwrap().inc_request_count("raft");

        self.rf_message_ch
            .send(PeerMessage::Message(req.clone()))
            .unwrap_or_else(|e| {
                error!("send message to raft error: {}", e);
            });
        let resp = RaftDone::new();
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn raft_conf_change(&mut self, ctx: RpcContext, req: ConfChangeReq, sink: UnarySink<RaftDone>) {
        self.metrics
            .lock()
            .unwrap()
            .inc_request_count("raft_conf_change");

        let cc = req.cc.clone().unwrap();
        let mut resp = RaftDone::new();
        let mut apply_req = ApplyReq::new();

        match cc.change_type {
            ConfChangeType::AddNode | ConfChangeType::AddLearnerNode => {
                apply_req.set_req_type(ReqType::Join);
                let mut join_req = JoinReq::new();
                join_req.set_client_id(cc.get_node_id());
                join_req.set_peer_id(cc.get_node_id());
                join_req.set_peer_addr(format!("{}:{}", req.ip, req.port));
                apply_req.set_join_req(join_req);
            }
            ConfChangeType::RemoveNode => {
                apply_req.set_req_type(ReqType::Leave);
                let mut leave_req = LeaveReq::new();
                leave_req.set_client_id(cc.get_node_id());
                leave_req.set_peer_id(cc.get_node_id());
                leave_req.set_peer_addr(format!("{}:{}", req.ip, req.port));
                apply_req.set_leave_req(leave_req);
            }
        }
        let (err, _) = self.start_op(&apply_req);
        match err {
            RespErr::OK => {
                let (sh, rh) = mpsc::sync_channel(0);
                {
                    let mut map = self.notify_ch_map.lock().unwrap();
                    map.insert(cc.get_node_id(), sh);
                }
                self.rf_message_ch
                    .send(PeerMessage::ConfChange(cc.clone()))
                    .unwrap();
                match rh.recv_timeout(Duration::from_millis(1000)) {
                    Ok(_) => resp.set_err(RespErr::OK),
                    Err(_) => resp.set_err(RespErr::ErrWrongLeader),
                }
            }
            _ => resp.set_err(RespErr::ErrWrongLeader),
        }

        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn probe(&mut self, ctx: RpcContext, req: ProbeReq, sink: UnarySink<ProbeResp>) {
        self.metrics.lock().unwrap().inc_request_count("probe");

        let mut ret = HashMap::new();
        ret.insert("health", "OK");

        let mut resp = ProbeResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(serde_json::to_string(&ret).unwrap());
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn peers(&mut self, ctx: RpcContext, req: PeersReq, sink: UnarySink<PeersResp>) {
        self.metrics.lock().unwrap().inc_request_count("peers");

        let mut resp = PeersResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(serde_json::to_string(&self.peers_addr.lock().unwrap().clone()).unwrap());
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn metrics(&mut self, ctx: RpcContext, req: MetricsReq, sink: UnarySink<MetricsResp>) {
        self.metrics.lock().unwrap().inc_request_count("metrics");

        let mut resp = MetricsResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(self.metrics.lock().unwrap().get_metrics());
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn get(&mut self, ctx: RpcContext, req: GetReq, sink: UnarySink<GetResp>) {
        self.metrics.lock().unwrap().inc_request_count("get");

        let t = Term::from_field_text(
            self.index
                .schema()
                .get_field(&self.unique_key_field_name)
                .unwrap(),
            req.get_doc_id(),
        );
        let tq = TermQuery::new(t, IndexRecordOption::Basic);
        let searcher = self.index.reader().unwrap().searcher();
        let top_docs = searcher.search(&tq, &TopDocs::with_limit(10)).unwrap();
        let mut doc = Document::default();
        for (_score, doc_address) in top_docs {
            doc = searcher.doc(doc_address).unwrap();
        }
        let named_doc = self.index.schema().to_named_doc(&doc);

        let mut resp = GetResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(serde_json::to_string(&named_doc).unwrap());
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn put(&mut self, ctx: RpcContext, req: ApplyReq, sink: UnarySink<PutResp>) {
        let (err, ret) = Self::start_op(self, &req);
        let mut resp = PutResp::new();
        resp.set_err(err);
        resp.set_value(ret);
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn delete(&mut self, ctx: RpcContext, req: ApplyReq, sink: UnarySink<DeleteResp>) {
        let (err, ret) = Self::start_op(self, &req);
        let mut resp = DeleteResp::new();
        resp.set_err(err);
        resp.set_value(ret);
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn commit(&mut self, ctx: RpcContext, req: ApplyReq, sink: UnarySink<CommitResp>) {
        let (err, ret) = Self::start_op(self, &req);
        let mut resp = CommitResp::new();
        resp.set_err(err);
        resp.set_value(ret);
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn rollback(&mut self, ctx: RpcContext, req: ApplyReq, sink: UnarySink<RollbackResp>) {
        let (err, ret) = Self::start_op(self, &req);
        let mut resp = RollbackResp::new();
        resp.set_err(err);
        resp.set_value(ret);
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn merge(&mut self, ctx: RpcContext, req: ApplyReq, sink: UnarySink<MergeResp>) {
        let (err, ret) = Self::start_op(self, &req);
        let mut resp = MergeResp::new();
        resp.set_err(err);
        resp.set_value(ret);
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn search(&mut self, ctx: RpcContext, req: SearchReq, sink: UnarySink<SearchResp>) {
        self.metrics.lock().unwrap().inc_request_count("search");

        let schema = self.index.schema();
        let default_fields: Vec<Field> = schema
            .fields()
            .flat_map(|(field, field_entry)| {
                if let FieldType::Str(text_field_options) = field_entry.field_type() {
                    if text_field_options.get_indexing_options().is_some() {
                        return Some(field);
                    }
                }
                None
            })
            .collect();

        let limit = req.get_from() + req.get_limit();

        let query_parser = QueryParser::for_index(&self.index, default_fields);
        let query = query_parser.parse_query(req.query.as_str()).unwrap();
        let searcher = self.index.reader().unwrap().searcher();
        let mut multi_collector = MultiCollector::new();
        let count_handle = if req.get_exclude_count() {
            None
        } else {
            Some(multi_collector.add_collector(Count))
        };
        let top_docs_handle = if req.get_exclude_docs() {
            None
        } else {
            Some(multi_collector.add_collector(TopDocs::with_limit(limit as usize)))
        };
        let facet_handle = if req.get_facet_field().is_empty() {
            None
        } else {
            let mut facet_collector = FacetCollector::for_field(schema.get_field(req.get_facet_field()).unwrap());
            for facet_prefix in req.get_facet_prefixes() {
                facet_collector.add_facet(facet_prefix);
            }
            Some(multi_collector.add_collector(facet_collector))
        };

//        // single field facet
//        let mut facet_collector = FacetCollector::for_field(schema.get_field(req.get_facet_field()).unwrap());
//        for facet_prefix in req.get_facet_prefixes() {
//            facet_collector.add_facet(facet_prefix);
//        }
//        let facet_handle = multi_collector.add_collector(facet_collector);

//        // multi field facet
//        let mut facet_data: HashMap<&str, Vec<String>> = HashMap::new();
//        for f in req.get_facets() {
//            let mut parts = f.split(':');
//            let field_name = parts.next().unwrap();
//            let field_value = parts.next().unwrap();
//            let mut field_values: Vec<String> = Vec::new();
//            if facet_data.contains_key(field_name) {
//                field_values = facet_data.get(field_name).unwrap().to_vec();
//            }
//            field_values.push(field_value.to_string());
//            facet_data.insert(field_name, field_values);
//        }
//        debug!("facet_data: {:?}", facet_data);
//        let mut facet_handles = HashMap::new();
//        for field_name in facet_data.keys() {
//            let field = schema.get_field(*field_name).unwrap();
//            let mut facet_collector = FacetCollector::for_field(field);
//            for field_value in facet_data.get(*field_name).unwrap() {
//                facet_collector.add_facet(field_value);
//            }
//            let facet_handle = multi_collector.add_collector(facet_collector);
//            facet_handles.insert(*field_name, facet_handle);
//        }

        // search index
        let mut multi_fruit = searcher.search(&query, &multi_collector).unwrap();

        // count
        let mut count: i64 = -1;
        if let Some(ch) = count_handle {
            count = ch.extract(&mut multi_fruit) as i64;
        }

        // docs
        let mut top_docs = Vec::new();
        if let Some(tdh) = top_docs_handle {
            top_docs = tdh.extract(&mut multi_fruit);
        }

        // facet
        let mut facet: HashMap<String, HashMap<String, u64>> = HashMap::new();
        if let Some(fh) = facet_handle {
            let facet_counts = fh.extract(&mut multi_fruit);
            let mut facet_kv: HashMap<String, u64> = HashMap::new();
            for facet_prefix in req.get_facet_prefixes() {
                for (facet_key, facet_value) in facet_counts.get(facet_prefix) {
                    debug!("{:?}={}", facet_key.to_string(), facet_value);
                    facet_kv.insert(facet_key.to_string(), facet_value);
                }
            }
            facet.insert(req.get_facet_field().to_string(), facet_kv);
        }

//        // single field facet
//        let facet_counts = facet_handle.extract(&mut multi_fruit);
//        for facet_prefix in req.get_facet_prefixes() {
//            for (facet_value, facet_count) in facet_counts.get(facet_prefix) {
//                debug!("{:?}={}", facet_value.to_string(), facet_count);
//            }
//        }

//        // multi field facet
//        for field_name in facet_handles.keys() {
//            let facet_counts = facet_handles.get(*field_name).unwrap().extract(&mut multi_fruit);
//            for facet_value in facet_data.get(*field_name).unwrap().to_vec() {
//                debug!("facet_value: {}", facet_value);
//                let facet_count = facet_counts.get(&facet_value);
//            }
//        }

        let mut docs: Vec<ScoredNamedFieldDocument> = Vec::new();
        let mut doc_pos: u64 = 0;
        for (score, doc_address) in top_docs {
            if doc_pos >= req.get_from() {
                let doc = searcher.doc(doc_address).unwrap();
                let named_doc = schema.to_named_doc(&doc);
                debug!(
                    "score: {:?} doc: {:?}",
                    score,
                    serde_json::to_string(&named_doc).unwrap()
                );

                let scored_doc = ScoredNamedFieldDocument {
                    fields: named_doc,
                    score,
                };
                docs.push(scored_doc);
            }
            doc_pos += 1;
        }

        let sr = SearchResult { docs, count, facet };

        let mut resp = SearchResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(serde_json::to_string(&sr).unwrap());
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }

    fn schema(&mut self, ctx: RpcContext, req: SchemaReq, sink: UnarySink<SchemaResp>) {
        self.metrics.lock().unwrap().inc_request_count("schema");

        let mut resp = SchemaResp::new();
        resp.set_err(RespErr::OK);
        resp.set_value(format!(
            "{}",
            serde_json::to_string(&self.index.schema()).unwrap()
        ));
        ctx.spawn(
            sink.success(resp)
                .map_err(move |e| error!("failed to reply {:?}: {:?}", req, e)),
        )
    }
}

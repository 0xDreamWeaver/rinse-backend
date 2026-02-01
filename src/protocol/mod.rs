pub mod messages;
pub mod client;
pub mod connection;
pub mod codec;
pub mod obfuscation;
pub mod file_selection;
pub mod peer;

pub use client::{
    SoulseekClient, SearchResult, DownloadProgress, DownloadStatus, DownloadResult,
    PendingDownload, DownloadInitResult, TransferComplete, TransferCompleteSender,
};
pub use messages::*;
pub use obfuscation::{obfuscate, deobfuscate};
pub use file_selection::{
    parse_query, normalize_for_matching, extract_track_name,
    score_title_match, score_file, is_audio_file, find_best_file, ScoredFile,
};
pub use peer::{
    send_peer_init, send_pierce_firewall, read_peer_init,
    FileConnection, IncomingConnection, handle_incoming_connection, handle_incoming_f_connection,
    connect_for_indirect_transfer, negotiate_download, download_file, NegotiationResult,
    queue_upload, wait_for_transfer_request,  // Modern QueueUpload method
    encode_string as peer_encode_string, send_message, read_message,
    ParsedSearchResponse, parse_file_search_response, connect_to_peer_for_results,
    receive_search_results_from_peer,
    ProgressCallback, ArcProgressCallback,
};

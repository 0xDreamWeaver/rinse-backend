# CLAUDE.md - Development Notes

## Project Overview
Rinse is a Soulseek P2P music download client with a Rust backend and React frontend.

---

## Session 2026-02-15 (Current)

### Context
Continued from previous session that ran out of context. Previous session implemented:
1. Search history feature (separate `search_history` table)
2. Search flow logging improvements (timing metrics, less aggressive early abort)
3. Discogs metadata provider integration

### Work Done This Session

#### 1. Verified Discogs Integration Compiles
- Fixed borrow-after-move error in `src/services/metadata/mod.rs:295` (was logging moved values)
- Backend compiles successfully with all previous changes

#### 2. Enhanced Discogs Logging
Added comprehensive logging to `src/services/metadata/discogs.rs`:
- **Rate limit tracking**: Logs `X-Discogs-Ratelimit-Remaining` header, warns if <10 remaining
- **Timing metrics**: Logs response time for API calls
- **Auth status**: Shows `authenticated=true/false` in search logs
- **Error context**: Better error messages with status, elapsed time, body

Key log patterns:
```
[Discogs] Searching for: 'query' (authenticated=true)
[Discogs] Rate limit: 58/60 remaining
[Discogs] Search response received in 0.45s
[Discogs] === 10 results for query: '...' ===
[Discogs]   #1: title='...', year=..., label=..., score=35
```

#### 3. Analyzed Download Failure Issue
User provided logs showing a problem:
```
First search: 56 users, 104 files → picked peakdilemma → "File not shared"
Retry search: 8 users, 9 files → picked peakdilemma AGAIN → failed again
```

**Root cause identified**: File selection has no memory of failed sources. On retry, it re-searches and picks the same failing peer.

**Additional observations**:
- Retry searches get dramatically fewer results (cached connections reused, server may throttle)
- First search had 65+ viable alternatives that were discarded
- "File not shared" means peer's index is stale - will never succeed for that file

#### 4. Implemented "Try Next Candidate" Feature

**Files Changed**:

`src/protocol/file_selection.rs`:
- Added `find_best_files()` - returns ALL scored candidates sorted by score (not just best)
- `find_best_file()` now wraps `find_best_files().into_iter().next()`

`src/protocol/client.rs`:
- Added import for `find_best_files`
- Added `get_best_files()` wrapper function

`src/services/queue.rs`:
- Added `pending_candidates: Arc<RwLock<HashMap<i64, Vec<ScoredFile>>>>` to `QueueService` struct
- Modified `execute_search_and_download()`:
  - Checks for cached candidates FIRST (before searching)
  - If cached candidates exist, uses next one (no re-search)
  - If no cached candidates, does fresh search and caches top 5 alternatives
- Modified `handle_transfer_completion()` error handling:
  - On failure with `retry_count >= 1`: checks if more cached candidates exist
  - If yes: marks for retry to try next candidate
  - If no: permanent failure, cleans up cached candidates
- Added cleanup of `pending_candidates` on success and permanent failure

**New behavior**:
```
Attempt 1: peakdilemma → "File not shared"
Attempt 2: disco_ [from cache] → tries next best
Attempt 3: mwav [from cache] → tries next
... up to 6 total attempts (1 + 5 cached)
```

**Logs to watch for**:
```
[QueueWorker] Caching 5 alternative candidates for queue_id=133
[QueueWorker] Using cached candidate #2 for queue_id=133 (4 remaining)
[QueueWorker] Selected '...' from 'disco_' (score=155.0, ...) [from cache]
[TransferMonitor] Transfer failed, trying next cached candidate: queue_id=133
[TransferMonitor] Transfer failed permanently (no more candidates): queue_id=133
```

### Current State
- All code compiles successfully (warnings only, no errors)
- User added `DISCOGS_TOKEN` to `.env` - Discogs integration ready to test
- "Try next candidate" feature ready to test
- Backend shows `discogs_configured=true` in startup logs

### Testing Needed
1. **Discogs metadata**: Search for a track and verify Discogs fallback works when MusicBrainz fails
2. **Try next candidate**: Trigger a "File not shared" failure and verify it tries next candidate without re-searching

---

## Previous Session Summary (2026-02-12)

### 1. Search History Feature
**Problem**: Searches weren't populating history (only "retry download" for failed items was).

**Solution**: Created separate `search_history` table.

**Files**:
- `migrations/010_create_search_history.sql` - New table
- `src/db/mod.rs` - Added `insert_search_history()`, updated `get_search_history()`
- `src/services/queue.rs` - Insert into history at 4 points (duplicate, search fail, transfer success, transfer fail)

**Design decision**: `search_queue` stays as fast working queue (deleted after processing), `search_history` is permanent record.

### 2. Search Flow Logging Improvements
**Problem**: Searches timing out despite results existing. Same searches work on retry.

**Changes** (`src/protocol/client.rs`):
- Added `first_connect_to_peer_time`, `first_result_time` timing metrics
- Increased `no_results_timeout` from 5s to 7s
- Early abort only if NO ConnectToPeer messages (was aborting with 0 results)
- Periodic progress logging every 3 seconds
- Enhanced final summary with timing

### 3. Discogs Metadata Provider
**Problem**: MusicBrainz not working well.

**Solution**: Added Discogs as fallback.

**Files**:
- `src/services/metadata/discogs.rs` (~500 lines) - Full API client
  - `search_release()`, `search_release_precise()`, `get_release_details()`
  - `clean_artist_name()` - removes "(2)" disambiguation
  - `parse_duration_to_ms()` - converts "5:23" to ms
  - Scoring algorithm with minimum threshold
  - Prefers `style` over `genre` (better for electronic music)
- `src/services/metadata/mod.rs` - Integrated as fallback

**Auth**: User token from Discogs Settings → Developers. Set `DISCOGS_TOKEN` env var.
- Authenticated: 60 req/min
- Unauthenticated: 25 req/min (search REQUIRES auth)

**Metadata priority**: MusicBrainz → Discogs → GetSongBPM → Spotify

---

## Architecture Reference

### Queue Service (`src/services/queue.rs`)
- `QueueService` struct holds `pending_candidates` cache
- `QueueWorker`: Processes search queue sequentially (Soulseek server limitation)
- `TransferMonitor`: Handles download completion/failure via channel
- Both insert into `search_history` before deleting from `search_queue`

### File Selection (`src/protocol/file_selection.rs`)
- `find_best_files()` - Returns Vec<ScoredFile> sorted by score
- `find_best_file()` - Returns Option<ScoredFile> (just first from above)
- `score_file()` - Considers title match, format (FLAC>WAV>MP3), bitrate, remix penalties
- `ScoredFile` struct has: username, filename, size, bitrate, score, peer_ip, peer_port

### Metadata Service (`src/services/metadata/mod.rs`)
- Orchestrates providers: MusicBrainz, Discogs, GetSongBPM, Spotify
- Merges data preferring existing values
- `lookup_and_store()` is main entry point

### Search Flow
1. Frontend → `search_queue` table
2. QueueWorker picks up → Soulseek search
3. `get_best_files()` → cache alternatives, pick best
4. `initiate_download()` (non-blocking, spawns task)
5. TransferMonitor receives completion
6. On fail: try next cached candidate or mark permanent
7. On success: metadata lookup, move to `search_history`

---

## Environment Variables
```bash
DISCOGS_TOKEN=xxx          # Discogs API token (required for search)
MUSICBRAINZ_APP_NAME=xxx   # App name for MusicBrainz user agent
GETSONGBPM_API_KEY=xxx     # GetSongBPM API key
SPOTIFY_CLIENT_ID=xxx      # Spotify credentials
SPOTIFY_CLIENT_SECRET=xxx
```

---

## Key Files Quick Reference
- `src/services/queue.rs` - Queue processing, retry logic, candidate caching
- `src/services/metadata/mod.rs` - Metadata orchestration
- `src/services/metadata/discogs.rs` - Discogs API client
- `src/protocol/client.rs` - Soulseek protocol, search, download
- `src/protocol/file_selection.rs` - File scoring and selection
- `src/db/mod.rs` - Database operations
- `migrations/010_create_search_history.sql` - Search history table

//! Attribution pages for third-party services
//!
//! These pages satisfy attribution requirements for APIs we use.

use axum::response::Html;

/// GetSongBPM attribution page
///
/// GET /getsongbpm
///
/// This page provides the required attribution for using the GetSongBPM API.
/// It includes a dofollow link back to getsongbpm.com as required by their
/// CC BY 4.0 license terms.
pub async fn getsongbpm_attribution() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>GetSongBPM Attribution - Rinse</title>
    <meta name="description" content="Rinse uses GetSongBPM for BPM and musical key data. Learn more about this service.">
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: #0a0a0a;
            color: #e5e5e5;
            min-height: 100vh;
            padding: 2rem;
        }
        .container { max-width: 800px; margin: 0 auto; }
        h1 { color: #00ff88; font-size: 2.5rem; margin-bottom: 0.5rem; }
        h2 { color: #00ff88; font-size: 1.5rem; margin: 2rem 0 1rem; }
        p { color: #888; line-height: 1.6; margin-bottom: 1rem; }
        .card {
            background: #111;
            border: 1px solid #333;
            padding: 1.5rem;
            margin-bottom: 1rem;
            border-radius: 4px;
        }
        .card h3 { color: #00ff88; margin-bottom: 0.5rem; }
        .card a { color: #00ff88; text-decoration: none; }
        .card a:hover { text-decoration: underline; }
        .services { display: grid; gap: 1rem; }
        @media (min-width: 600px) {
            .services { grid-template-columns: repeat(2, 1fr); }
        }
        .footer { text-align: center; margin-top: 3rem; padding-top: 2rem; border-top: 1px solid #333; }
        .footer a { color: #00ff88; }
    </style>
</head>
<body>
    <div class="container">
        <h1>About Rinse</h1>
        <p>Information about this application and the services it uses</p>

        <div class="card">
            <h3>Rinse</h3>
            <p>A modern Soulseek client for music discovery and collection management</p>
            <p>
                Rinse is a self-hosted music download manager that connects to the Soulseek
                peer-to-peer network. It provides a clean, modern interface for searching,
                downloading, and organizing your music collection.
            </p>
            <p>
                Features include queue-based downloads, automatic metadata enrichment,
                file tag writing, and real-time progress tracking via WebSocket updates.
            </p>
        </div>

        <h2>Powered By</h2>
        <p>Rinse relies on the following services for its functionality.</p>

        <div class="services">
            <div class="card">
                <h3><a href="https://www.slsknet.org/" target="_blank" rel="noopener">Soulseek</a></h3>
                <p>Peer-to-peer file sharing network for music discovery and sharing</p>
            </div>
            <div class="card">
                <h3><a href="https://musicbrainz.org/" target="_blank" rel="noopener">MusicBrainz</a></h3>
                <p>Open music encyclopedia for track metadata including artist, album, title, and more</p>
                <p><small>License: CC0 / CC BY-NC-SA 3.0</small></p>
            </div>
            <div class="card">
                <h3><a href="https://coverartarchive.org/" target="_blank" rel="noopener">Cover Art Archive</a></h3>
                <p>Free and open repository of album artwork, linked with MusicBrainz releases</p>
            </div>
            <div class="card">
                <h3><a href="https://getsongbpm.com/" target="_blank" rel="noopener">GetSongBPM</a></h3>
                <p>BPM and musical key database for DJs and music producers</p>
                <p><small>License: CC BY 4.0</small></p>
            </div>
        </div>

        <h2>Technical Details</h2>
        <div class="card">
            <p><strong>Backend:</strong> Rust / Axum</p>
            <p><strong>Frontend:</strong> React / TypeScript</p>
            <p><strong>Database:</strong> SQLite</p>
            <p><strong>Protocol:</strong> Soulseek P2P</p>
        </div>

        <div class="footer">
            <p>BPM and key data provided by <a href="https://getsongbpm.com" target="_blank" rel="noopener">GetSongBPM</a></p>
        </div>
    </div>
</body>
</html>"#)
}

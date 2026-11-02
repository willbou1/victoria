<img src="https://raw.githubusercontent.com/willbou1/victoria/master/assets/header.png" width="750" height="300" alt="banner">

**Victoria** is a BitTorrent client written in Rust with the intention
of deeply exploring the BitTorrent protocol for my own satisfaction
and curiosity.

The project includes a custom implementation of the BitTorrent peer
protocol and Mainline DHT, with an asynchronous architecture powered
by Tokio. A TUI framework called whodatrow was also custom-built for
easy visualisation of all the client state. It makes deugging easy
while being pleasant on the eyes.

# Features
## BitTorrent
| Feature | Status | Receive | Send | Notes |
|---|---|---|---|---|
| Bencode parser | ✅ | | | |
| Torrent file parsing | ✅ | | | Lacks nodes and HTTP seeds |
| Magnet link support | ✅ | | | Lacks nodes |
| UDP trackers | ✅ | ✅ | ❌ | |
| HTTP trackers | ✅ | ✅ | ❌ | |
| Peer wire protocol | ✅ | ✅ | ✅ | |
| Metadata exchange | ✅ | ✅ | ✅ | |
| PEX | ✅ | ✅ | ❌ | |
| uTP | ❌ | | | |
| Piece transfer | ✅ | ✅ | ✅ | Lacks end-game and opportunistic unchoking|
| Piece verification | ✅ | | | |
| Sequential downloading | ✅ | | | |
| File priorities | ✅ | | | |

## DHT
| Feature | Status |
|---|---|
| Bootstrapping | ✅ |
| Peer discovery | ✅ |
| Maintenance | ❌ |
| Contributing | ❌ |

# Usage
Launch the TUI:
```bash
victoria
```
Torrents will be under victoria in your download folder.

# Building
Clone the repository:
```bash
git clone <repository-url>
cd victoria
```
Build the project:
```bash
cargo build --release
```

# License
Victoria is licensed under the MIT License.

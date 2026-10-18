# Parallel streams behind a firewall

How to make `--streams`, `--transport tcp` and `--transport quic` work while the firewall
stays on, with nothing for users to configure.

## The problem

With the firewall on, the parallel streams never connect, and barqsync falls back to its
single ssh connection:

```text
$ barqsync -avv --info=progress2 --streams 4 big.zip me@server:projTest/
parallel streams: could not connect to 192.168.1.4:52047 (Connection reset by peer (os error 54)), using the ssh connection
big.zip
     65,011,712   1%      5.16MB/s    0:10:09 (xfr#0, ir-chk=1/1)
```

**Why:** for `--streams` (and `--transport tcp`), the barqsync on the other machine opens a
**new TCP port** with a random number and waits for the extra connections there.
`--transport quic` opens a random **UDP port**. Those connections go straight to that port,
outside ssh, so every kind of firewall stops them:

| Where | What happens |
| --- | --- |
| macOS firewall | Blocks apps that aren't on its list: the connection is reset at once |
| Linux (ufw, firewalld) | Blocks ports that aren't open: the connection times out after 5 s |
| Cloud servers | Usually only port 22 (ssh) is open |
| Routers, NAT, jump hosts (`ProxyJump`) | The other machine isn't directly reachable at all |

The transfer still works over the single ssh connection, but without the speedup:

- **On Wi-Fi:** between two Macs on 2.4 GHz Wi-Fi, one ssh connection ran at about
  4.5 MB/s. 4 parallel streams, with the firewall turned off for the test, ran at
  **6.1 MB/s, about 35% faster**.
- **On a cable:** one ssh connection is limited by how fast one CPU core can encrypt.

### Which features are affected

| Feature | Works with the firewall on? |
| --- | --- |
| Plain ssh (the default), `-z`, `--strategy adaptive` | Yes |
| `--streams N`, `--transport tcp` | **No**: falls back to one ssh connection |
| `--transport quic` | **No**: falls back to one ssh connection |

## Goal

Every feature works with the firewall on:

- no new open ports;
- no firewall rules for users to add;
- no weaker security than today.

## Options

| Option | Works with firewall on | Faster | What users must do | Platforms |
| --- | --- | --- | --- | --- |
| **A. Parallel ssh connections** (recommended) | **Yes**, uses port 22, already open | **Yes** | Nothing (ssh keys, as for any script) | macOS, Linux, jump hosts |
| B. Signed and notarized macOS builds | Yes, while "allow signed software" stays on (the default) | Yes | Install a release build, not `cargo install` | macOS only |
| C. A fixed port (`--streams-port P`) | Only after someone opens that port | Yes | Add one firewall rule | Servers they administer |
| D. Tunnel the streams through the one ssh connection | Yes | **No**, still one connection and one encryption thread | Needs `AllowTcpForwarding` | — |
| E. The server connects back to the client | **No**, the client's firewall or NAT blocks it instead | — | — | — |
| F. Turn the firewall off | — | — | Not acceptable | — |

A solves the problem everywhere ssh works. B and C are useful extras for the TLS and QUIC
transports. D and E don't solve it.

## Recommended: parallel ssh connections

Instead of opening a new port, barqsync opens a few more **ssh connections**. A firewall
that lets ssh in lets all of them in.

```text
your Mac                                        the other machine
                                                (all on port 22)
barqsync ──── ssh #0: control and data ───────▶ sshd ──▶ barqsync --server   (main)
         ──── ssh #1: data ───────────────────▶ sshd ──▶ barqsync --join ─┐
         ──── ssh #2: data ───────────────────▶ sshd ──▶ barqsync --join ─┼──▶ local socket ──▶ main
         ──── ssh #3: data ───────────────────▶ sshd ──▶ barqsync --join ─┘     (not network)
```

### How it works

1. **Offer.** The main server creates a private local socket: a Unix socket in a new
   directory only the user can open, so the firewall doesn't apply to it. It also makes a
   random 32-byte token, then sends the socket's path and the token over ssh #0.
2. **Connect.** The client starts N−1 more connections using **the same ssh command**, so
   `-e`, ports, keys, `ProxyJump` and ciphers all carry over. Each one runs
   `barqsync --server --join PATH TOKEN`. They start with `-o BatchMode=yes`, so they never
   ask for a password.
3. **Join.** Each join process connects to the local socket, presents the token, and hands
   its ssh input and output over to the main server (Unix file-descriptor passing). It then
   waits until the transfer ends.
4. **Use.** From then on the main server treats them exactly like today's TLS streams: each
   file travels on one stream, and small-file packs are spread across streams. No TLS layer
   is needed, because ssh already encrypts and authenticates each connection.
5. **Fall back.** A connection that fails is skipped. If none join within 5 s, barqsync
   uses the single ssh connection, as it does today, and `-v` says why.

### Why it's faster

- **Separate TCP connections:** on Wi-Fi or a lossy link, a lost packet slows only one of
  them (the 35% measured above).
- **Encryption on several cores at both ends:** every ssh client and every sshd is its own
  process, so a cable is no longer limited to one core.

### Security

- No listening network port anywhere, so there is nothing for a firewall to allow.
- Every connection is an ordinary, authenticated ssh login.
- The local socket sits in a private directory, owner only, and needs the 32-byte token.
  Both are removed when the transfer ends.
- The self-signed certificates of the TLS transport aren't needed for this path.

### Details and limits

- **Password logins:** each extra connection would ask again. Started with `BatchMode=yes`,
  they fail quietly instead, barqsync carries on with one connection, and `-v` suggests
  setting up keys (`ssh-copy-id me@server`).
- **Startup cost:** each ssh login takes roughly 0.1–0.5 s. The logins run in parallel while
  the scan runs, and only when there is enough to send (for example 64 MiB or more), so
  small syncs don't pay for them.
- **sshd limits:** by default sshd starts refusing some new logins once 10 are still
  logging in at the same time (`MaxStartups 10:30:100`), so up to 8 streams by default.
- **Protocol:** the new messages need protocol version 8, so this ships as **0.3.0**. As
  today, both ends need the same release line (0.3.x with 0.3.x).

### Proposed behaviour

| Option | Behaviour |
| --- | --- |
| `--streams N` | N parallel ssh connections (works wherever ssh works) |
| `--transport tcp` | Today's TLS streams, for networks where a port is reachable; plus `--streams-port P` for a fixed port (option C) |
| `--transport quic` | Unchanged (needs UDP); `--streams-port P` applies too |

If not every connection can join, barqsync uses the ones that did. If none can, it uses the
single ssh connection.

### Implementation plan

1. **Protocol:** a `Msg::SshStreamOffer { path, token }` message from the server; reuse
   `Msg::StreamsReady` for the answer; raise `VERSION` to 8.
2. **Server:** create the private directory and socket, accept joins that present the token,
   and receive their input and output with `SCM_RIGHTS` (`libc::sendmsg`/`recvmsg`; the
   crate already uses libc). Wrap each pair as a `Stream` (`Read + Write`) and pass them to
   the existing `attach()` in `src/remote/streams.rs`.
3. **Join mode:** `barqsync --server --join PATH TOKEN` connects, sends the token, passes its
   input and output (fds 0 and 1), then blocks until the socket closes.
4. **Client:** start the extra remote-shell commands with the argv `spawn_server` already
   builds (`src/remote/client.rs`), add `-o BatchMode=yes` when the program is OpenSSH, wait
   up to 5 s, and report with `-vv` how many joined.
5. **Tests:** use the fake-ssh method from the 0.2.2 work, which needs no network. Cover all
   joins, some failing, a wrong token, Ctrl-C during setup, and pull as well as push.
6. **Benchmarks:** one ssh connection, against parallel ssh connections, against TLS
   streams, on Wi-Fi and on a cable.
7. **Docs:** update the README ("Made for real networks", options table) and add a section
   to `IMPLEMENTATION.md`.

## Until then: what users can do

### Keep the default

One ssh connection always works, firewall or not. `-z` and `--strategy adaptive` work over
it too.

### macOS: put barqsync on the firewall's list

Run this on the machine that **receives** the connections, and again after every reinstall,
because each build is a new binary:

```bash
F=/usr/libexec/ApplicationFirewall/socketfilterfw
B=$HOME/.cargo/bin/barqsync
codesign --force -s - "$B"
sudo $F --add "$B"
sudo $F --unblockapp "$B"
sudo $F --listapps | grep -A1 barqsync    # should say: Allow incoming connections
```

- **Not confirmed yet:** in the test above, `--add` and `--unblockapp` without the
  `codesign` step still got "Connection reset". Only turning the firewall off made the
  streams connect. Check with `-vv` for `using 4 parallel TLS data streams`.
- **Overridden by "Block all incoming connections":** if that's on in System Settings →
  Network → Firewall → Options, nothing on the list gets through.

### Linux

The port is random today, so only a broad rule works:

```bash
sudo ufw allow from 192.168.1.0/24 proto tcp
```

This opens every TCP port to the local network. It's acceptable on a trusted home network
only, never on a machine reachable from the internet. A fixed `--streams-port` (option C)
would allow a narrow rule instead.

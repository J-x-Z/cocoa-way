#!/usr/bin/env python3
"""Bounded V2 relay regressions using production Perl and local sockets only.

Reuse the stdio suite's wire, FIFO, pressure and cleanup assertions with a Unix
transport. No containers, Waypipe, GUI, network access or repository outputs.
"""

import argparse
import importlib.util
import os
from pathlib import Path
import re
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("stdio", ROOT / "scripts/test-stdio-backpressure.py")
stdio = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stdio)


def extract(source):
    matches = re.findall(
        r'fn guest_publish_socket_relay_perl\(\) -> &\x27static str \{\s*r#"(.*?)"#\s*\}',
        source, re.S,
    )
    assert len(matches) == 1, "cannot identify production V2 script"
    return matches[0]


SCRIPT = extract((ROOT / "src/container_sessions.rs").read_text())
BaseRelay = stdio.Relay


class Relay(BaseRelay):
    def __init__(self, command="import time; time.sleep(60)"):
        self.directory = tempfile.TemporaryDirectory(prefix="cw-v2-", dir="/tmp")
        self.path = Path(self.directory.name) / "p"
        transport_path = Path(self.directory.name) / "t"
        env = dict(os.environ, COCOA_WAY_RELAY_TEST_STATS="1",
                   XDG_RUNTIME_DIR=self.directory.name)
        env.pop("COCOA_WAY_AUDIO_SOCKET", None)
        self.process = subprocess.Popen(
            ["perl", "-e", SCRIPT, str(self.path), str(transport_path),
             sys.executable, "-c", command],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, bufsize=0, start_new_session=True,
        )
        self.peers = []
        self.stderr = b""
        self.transport = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        for pipe in (self.process.stdout, self.process.stderr):
            os.set_blocking(pipe.fileno(), False)
        try:
            ready = b"COCOA_WAY_TRANSPORT_V2_READY\n"
            assert BaseRelay.read(self, len(ready)) == ready
            self.transport.connect(str(transport_path))
            self.transport.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            self.transport.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
            self.transport.setblocking(False)
            for byte in b"CWV2\x01":
                self.write(bytes([byte]))
        except BaseException:
            self.__exit__()
            raise

    def __exit__(self, *args):
        try:
            self.transport.close()
        finally:
            super().__exit__(*args)

    def write(self, data, timeout=5):
        deadline = time.monotonic() + timeout
        offset = 0
        while offset < len(data):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([], [self.transport], [], max(remaining, 0))[1]:
                raise TimeoutError("fixture socket write deadline")
            try:
                count = self.transport.send(memoryview(data)[offset:offset + 65536])
                if count == 0:
                    raise EOFError("transport closed while writing")
                offset += count
            except BlockingIOError:
                continue

    def read(self, size, timeout=4):
        deadline = time.monotonic() + timeout
        data = bytearray()
        while len(data) < size:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.transport], [], [], max(remaining, 0))[0]:
                raise TimeoutError("fixture socket read deadline")
            try:
                part = self.transport.recv(min(size - len(data), 65536))
            except BlockingIOError:
                continue
            if not part:
                raise EOFError("transport EOF")
            data.extend(part)
        return bytes(data)

    def close_input(self):
        self.transport.shutdown(socket.SHUT_WR)

    def stats(self):
        match = re.search(rb"cocoa-way transport v2 stats: ([^\n]+)", self.stderr)
        assert match, self.stderr.decode(errors="replace")
        stats = {key.decode(): int(value) for key, value in
                 (field.split(b"=") for field in match[1].split())}
        assert stats["output_high"] <= 256 * 1024, stats
        assert stats["peer_high"] <= 256 * 1024, stats
        assert stats["input_high"] <= 4 * 1024 * 1024, stats
        assert stats["output_frames_high"] <= 4096, stats
        assert stats["peer_chunks_high"] <= 5, stats
        stats["partial_writes"] = stats["transport_partial"] + stats["peer_partial"]
        return stats


stdio.Relay = Relay
stdio.SCRIPT = SCRIPT


class PublishBackpressure(stdio.StdioBackpressure):
    require_partial_writes = False

    # Only stdio carries frames on stdin/stdout. V2 uses a separate socket.
    test_child_diagnostics_do_not_share_framed_stdio = None

    def test_closed_stdout_and_uncooperative_child_have_bounded_teardown(self):
        command = ("import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); "
                   "print('child ready',flush=True); time.sleep(60)")
        with Relay(command) as relay:
            self.assertEqual(BaseRelay.read(relay, 12), b"child ready\n")
            relay.peer()
            relay.close_input()
            self.assertEqual(relay.wait(timeout=3), 128 + signal.SIGKILL)


class PublishProgress(unittest.TestCase):
    def test_slow_peer_keeps_progress_and_other_peer_live_past_ten_seconds(self):
        with Relay() as relay:
            a, aid = relay.peer()
            b, bid = relay.peer()
            relay.frame(2, aid, b'a' * 65536)
            relay.frame(2, aid, b'b' * 65536)
            received = bytearray()
            started = time.monotonic()
            while len(received) < 131072:
                self.assertLess(time.monotonic() - started, 18)
                chunk = a.recv(min(4096, 131072 - len(received)))
                self.assertTrue(chunk, 'progressing peer disconnected')
                received.extend(chunk)
                b.sendall(b'tick')
                self.assertEqual(relay.receive(), (2, bid, b'tick'))
                time.sleep(.4)
            self.assertGreater(time.monotonic() - started, 10)
            self.assertEqual(received, b'a' * 65536 + b'b' * 65536)
            relay.finish()

    def test_partial_frame_progress_survives_past_ten_seconds(self):
        with Relay() as relay:
            a, aid = relay.peer()
            relay.write(stdio.HEADER.pack(2, aid, 12))
            started = time.monotonic()
            for value in range(12):
                relay.write(bytes([value]))
                self.assertEqual(stdio.read_socket(a, 1), bytes([value]))
                time.sleep(1)
            self.assertGreater(time.monotonic() - started, 10)
            relay.finish()


class PublishLifecycle(unittest.TestCase):
    def test_term_allows_cooperative_point_six_second_cleanup(self):
        command = """import signal,time,sys
def cleanup(signum,frame):
    print('cleanup started',flush=True)
    time.sleep(.6)
    print('cleanup finished',flush=True)
    sys.exit(0)
signal.signal(signal.SIGTERM,cleanup)
print('ready',flush=True)
time.sleep(60)
"""
        with Relay(command) as relay:
            self.assertEqual(BaseRelay.read(relay, 6), b'ready\n')
            started = time.monotonic()
            relay.close_input()
            self.assertEqual(relay.wait(timeout=3), 0)
            self.assertEqual(BaseRelay.read(relay, 33), b'cleanup started\ncleanup finished\n')
            self.assertGreaterEqual(time.monotonic() - started, .6)

    def test_natural_child_failure_and_signal_are_not_success(self):
        for command, code in (("import sys; sys.exit(23)", 23),
                              ("import os,signal; os.kill(os.getpid(),signal.SIGTERM)", 143)):
            with self.subTest(code=code), Relay(command) as relay:
                self.assertEqual(relay.wait(timeout=3), code)


class PublishAdmission(unittest.TestCase):
    def check_admission(self, setup, expected_size, expected_data):
        imports = SCRIPT[:SCRIPT.index("my $sock_path = shift @ARGV;")]
        state = SCRIPT[SCRIPT.index("my $chunk_size ="):SCRIPT.index("RELAY: while (1) {")]
        start = SCRIPT.index("  for my $p (values %peers) {\n    watch(")
        watch = SCRIPT[start:SCRIPT.index("\n  $! = 0;", start)]
        start = SCRIPT.index("\n    my $id = $peer_ids{fileno($fh)};") + 1
        read = SCRIPT[start:SCRIPT.index("\n  }\n}\nfor my $id", start)]
        script = imports + r"""
use Socket qw(AF_UNIX SOCK_STREAM PF_UNSPEC);
socketpair(my $server, my $writer, AF_UNIX, SOCK_STREAM, PF_UNSPEC) or die $!;
my $transport = $server;
""" + state + setup + f"""
my $expected_size = {expected_size};
my $expected_data = pack('H*', '{expected_data.hex()}');
""" + r"""
my @before = map { [@$_] } @output;
my $before_bytes = $output_bytes;
$peers{7} = { fh => $server, queue => [], offset => 0, bytes => 0, closing => 0, notify => 0 };
$peer_ids{fileno($server)} = 7;
syswrite($writer, 'tiny') == 4 or die 'fixture write';
die 'wrong size' unless peer_read_size() == $expected_size;
my $poll = IO::Poll->new();
""" + watch + r"""
die 'wrong poll admission' unless !!(($poll->mask($server) || 0) & POLLIN) == ($expected_size > 0);
for my $fh ($server) {
""" + read + r"""
}
die 'spurious close' if $peers{7}{closing};
die 'overflow' if $output_bytes > $output_limit || @output > $output_frames_limit;
for my $i (0 .. $#before) {
  die 'FIFO/age changed' unless $output[$i][0] eq $before[$i][0] && $output[$i][1] == $before[$i][1];
}
my $added = length($expected_data) ? 1 : 0;
die 'frame count' unless @output == @before + $added;
die 'byte count' unless $output_bytes == $before_bytes + ($added ? 9 + length($expected_data) : 0);
die 'cursor' unless $read_cursor == ($added ? 8 : 0);
die 'DATA' if $added && $output[-1][0] ne pack('CNN', 2, 7, length($expected_data)) . $expected_data;
if (length($expected_data) < 4) {
  my $left = '';
  my $n = sysread($server, $left, 4);
  die 'lost bytes' unless defined $n && $left eq substr('tiny', length($expected_data));
}
print "ok\n";
"""
        result = subprocess.run(["perl", "-e", script], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stdout, b"ok\n")

    test_three_full_frames_admit_tiny_peer_without_draining_output = (
        stdio.StdioReadAdmission.test_three_full_frames_admit_tiny_peer_without_draining_output)
    test_no_header_or_payload_capacity_does_not_read_or_close_peer = (
        stdio.StdioReadAdmission.test_no_header_or_payload_capacity_does_not_read_or_close_peer)
    test_one_payload_byte_fills_capacity_without_overflow = (
        stdio.StdioReadAdmission.test_one_payload_byte_fills_capacity_without_overflow)

    def test_full_frame_limit_does_not_read(self):
        self.check_admission("send_frame(2, 1, '') for 1 .. $output_frames_limit;\n", 0, b"")


class PublishQueues(unittest.TestCase):
    def run_fixture(self, body):
        imports = SCRIPT[:SCRIPT.index("my $sock_path = shift @ARGV;")]
        state = SCRIPT[SCRIPT.index("my $chunk_size ="):SCRIPT.index("RELAY: while (1) {")]
        result = subprocess.run(["perl", "-e", imports + r"""
use Socket qw(AF_UNIX SOCK_STREAM PF_UNSPEC SOL_SOCKET SO_SNDBUF);
socketpair(my $server, my $reader, AF_UNIX, SOCK_STREAM, PF_UNSPEC) or die $!;
my $transport = $server;
setsockopt($server, SOL_SOCKET, SO_SNDBUF, pack('i', 4096)) or die $!;
""" + state + body], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))

    def test_partial_writes_keep_allocation_charged_and_refresh_only_active_lane(self):
        start = SCRIPT.index("  for my $fh ($poll->handles(POLLOUT)) {")
        writes = SCRIPT[start:SCRIPT.index("\n  # Rotate peer reads", start)]
        for lane in ("output", "peer"):
            with self.subTest(lane=lane):
                self.run_fixture(r"""
$peers{7} = { fh => $server, queue => [['x' x $chunk_size, now() - 1]],
  offset => 0, bytes => $chunk_size, closing => 0, notify => 0 };
$peer_ids{fileno($server)} = 7;
$input_bytes = $chunk_size;
send_frame(2, 7, 'x' x $chunk_size);
my $before_output = $output_bytes;
my $output_age = $output[0][1];
my $peer_age = $peers{7}{queue}[0][1];
my $poll = IO::Poll->new();
$poll->mask($server => POLLOUT);
$poll->poll(0);
die 'fixture not writable' unless $poll->events($server) & POLLOUT;
""" + ("$transport = $reader;\n" if lane == "peer" else "") + "RELAY: {\n" + writes + r"""
}
die 'output allocation released early' unless $output_bytes == $before_output;
die 'peer allocation released early' unless $input_bytes == $chunk_size && $peers{7}{bytes} == $chunk_size;
""" + ("die 'output clock changed' unless $output[0][1] == $output_age;\n"
       "die 'peer progress not refreshed' unless $peers{7}{queue}[0][1] > $peer_age;\n"
       "die 'partial write not exercised' unless $peers{7}{offset} > 0 && $peers{7}{offset} < $chunk_size;\n"
       if lane == "peer" else
       "die 'peer clock changed' unless $peers{7}{queue}[0][1] == $peer_age;\n"
       "die 'output progress not refreshed' unless $output[0][1] > $output_age;\n"
       "die 'partial write not exercised' unless $output_offset > 0 && $output_offset < $chunk_size;\n"))

    def test_queue_deadlines_do_not_depend_on_incomplete_frame(self):
        start = SCRIPT.index("  my @expires;")
        expiry = SCRIPT[start:SCRIPT.index("  my $poll = IO::Poll->new();", start)]
        for setup in (
            "send_frame(2, 7, 'x'); $output[0][1] = now() - 11;",
            "$peers{7} = { queue => [['x', now() - 11]] };",
        ):
            with self.subTest(setup=setup):
                self.run_fixture(setup + "\nRELAY: {\n" + expiry + r"""
die 'expired queue did not stop relay';
}
die 'deadline was not an error' unless $exit_code == 1;
""")


def main():
    global SCRIPT
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--baseline-ref")
    parser.add_argument("--negative", choices=("admission", "uncapped", "deadline", "teardown"))
    args, rest = parser.parse_known_args()
    if args.baseline_ref:
        SCRIPT = extract(subprocess.check_output(
            ["git", "show", f"{args.baseline_ref}:src/container_sessions.rs"], cwd=ROOT, text=True))
    if args.negative:
        replacements = {
            "admission": ("if (peer_read_size()) {",
                          "if ($output_bytes + $chunk_size + 9 <= $output_limit) {"),
            "uncapped": ("sysread($fh, $data, $size)", "sysread($fh, $data, $chunk_size)"),
            "deadline": ("my $deadline = 10;", "my $deadline = 60;"),
            "teardown": ("my $status = stop_child($pid);",
                         "kill 'TERM', $pid; waitpid($pid, 0); my $status = 0;"),
        }
        old, new = replacements[args.negative]
        assert SCRIPT.count(old) == 1, "negative control must modify exactly one production site"
        SCRIPT = SCRIPT.replace(old, new)
    stdio.SCRIPT = SCRIPT
    unittest.main(argv=[sys.argv[0], *rest], verbosity=2)


if __name__ == "__main__":
    main()

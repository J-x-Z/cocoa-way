#!/bin/bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--display SLOT] [waypipe arguments...]" >&2
    echo "       $0 --list-displays" >&2
    echo "Example: $0 ssh user@linux-host firefox" >&2
    echo "         $0 --display display-1 ssh user@linux-host niri" >&2
}

selected_slot="${COCOA_WAY_DISPLAY_SLOT:-}"
list_displays=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --display)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "Error: --display requires a display slot name." >&2
                usage
                exit 2
            fi
            selected_slot="$2"
            shift 2
            ;;
        --list-displays)
            list_displays=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            break
            ;;
    esac
done

runtime_candidates=()
if [ -n "${COCOA_WAY_RUNTIME_DIR:-}" ]; then
    runtime_candidates+=("$COCOA_WAY_RUNTIME_DIR")
fi
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    runtime_candidates+=("$XDG_RUNTIME_DIR")
fi
if [ -n "${TMPDIR:-}" ]; then
    runtime_candidates+=("${TMPDIR%/}/cocoa-way")
fi
runtime_candidates+=("/tmp/cocoa-way")

# macOS ships Perl. Its nonblocking, no-follow opens avoid shell redirections/cat
# blocking on a replaced FIFO, while fstat verifies the object actually opened.
discover_displays() {
    /usr/bin/perl - "$list_displays" "$selected_slot" \
        "${XDG_RUNTIME_DIR:-}" "${WAYLAND_DISPLAY:-}" "${runtime_candidates[@]}" <<'PERL'
use strict;
use warnings;
use Fcntl qw(:DEFAULT :mode);
use Cwd qw(abs_path);
use File::Basename qw(dirname);
my ($list, $selected, $xdg, $preferred, @candidates) = @ARGV;
my $uid = $>;
sub safe_text { defined($_[0]) && $_[0] !~ /[\x00-\x1f\x7f]/ }
sub directory {
    my ($path) = @_;
    return unless safe_text($path);
    my @s = lstat($path);
    return unless @s && S_ISDIR($s[2]) && $s[4] == $uid && !($s[2] & 0077);
    my $original = dirname($path);
    while (1) {
        my @p = lstat($original);
        return unless @p;
        if (S_ISLNK($p[2])) {
            return unless $p[4] == 0;
        } else {
            return unless S_ISDIR($p[2]) && ($p[4] == 0 || $p[4] == $uid);
            return if ($p[2] & 0022) && !($p[4] == 0 && ($p[2] & 01000));
        }
        my $next = dirname($original);
        last if $next eq $original;
        $original = $next;
    }
    my $parent = abs_path("$path/..");
    return unless defined $parent;
    while (1) {
        my @p = lstat($parent);
        return unless @p && S_ISDIR($p[2]) && ($p[4] == 0 || $p[4] == $uid);
        return if ($p[2] & 0022) && !($p[4] == 0 && ($p[2] & 01000));
        last if $parent eq '/';
        $parent =~ s{/[^/]+$}{};
        $parent = '/' if $parent eq '';
    }
    return \@s;
}
sub unchanged {
    my ($path, $before) = @_;
    my $after = directory($path);
    return $after && $before->[0] == $after->[0] && $before->[1] == $after->[1];
}
sub marker {
    my ($path) = @_;
    sysopen(my $fh, $path, O_RDONLY | O_NONBLOCK | O_NOFOLLOW) or return;
    my @s = stat($fh);
    return unless @s && S_ISREG($s[2]) && $s[3] == 1 && $s[4] == $uid
        && !($s[2] & 0022) && $s[7] <= 256;
    my $text = '';
    while (length($text) <= 256) {
        my $n = sysread($fh, my $chunk, 257 - length($text));
        return unless defined $n;
        last if $n == 0;
        $text .= $chunk;
    }
    return if length($text) > 256;
    $text =~ s/\s+\z//;
    return safe_text($text) ? $text : undef;
}
sub live_process {
    my ($pid) = @_;
    return unless defined($pid) && $pid =~ /\A[0-9]+\z/ && $pid > 0 && $pid <= 2147483647;
    return unless kill(0, $pid);
    open(my $ps, '-|', 'ps', '-p', $pid, '-o', 'uid=', '-o', 'command=') or return;
    my $line = <$ps> // '';
    close($ps);
    return $line =~ /^\s*([0-9]+)\s+.*cocoa-way/ && $1 == $uid;
}
sub socket_name {
    my ($path, $preferred) = @_;
    my @names;
    if (defined($preferred) && $preferred ne '') {
        @names = ($preferred);
    } else {
        opendir(my $dir, $path) or return;
        @names = sort grep { /^wayland-/ } readdir($dir);
        closedir($dir);
    }
    for my $name (@names) {
        next unless safe_text($name) && $name ne '.' && $name ne '..' && $name !~ m{/};
        my @s = lstat("$path/$name");
        return $name if @s && S_ISSOCK($s[2]) && $s[4] == $uid && !($s[2] & 0022);
    }
    return;
}
sub emit_display {
    my ($slot, $path, $preferred) = @_;
    $path =~ s{/+\z}{};
    my $before = directory($path) or return;
    my $name = socket_name($path, $preferred);
    return unless defined($name) && unchanged($path, $before);
    print "$slot\t$path\t$name\n";
    return 1;
}
if ($list eq 'true' || $selected eq '' || $selected eq 'default' || $selected eq 'auto') {
    my $found = $xdg ne '' && $preferred ne '' && emit_display('default', $xdg, $preferred);
    for my $path (@candidates) {
        last if $found;
        $found = emit_display('default', $path, undef);
    }
    exit if $list ne 'true';
}
opendir(my $tmp, '/tmp') or exit;
my @managed = sort grep { /^cwd-/ } readdir($tmp);
closedir($tmp);
for my $name (@managed) {
    my $path = "/tmp/$name";
    my $before = directory($path) or next;
    my $slot = marker("$path/display.slot");
    next unless defined($slot) && $slot ne '' && ($list eq 'true' || $slot eq $selected);
    next unless live_process(marker("$path/display.parent")) && live_process(marker("$path/display.worker"));
    next unless unchanged($path, $before);
    last if emit_display($slot, $path, undef) && $list ne 'true';
}
PERL
}

runtime_dir=""
display=""
discovered=$(discover_displays)
if $list_displays; then
    printf '%-18s %s\n' "SLOT" "SOCKET"
    while IFS=$'\t' read -r slot candidate socket; do
        [ -n "$socket" ] || continue
        printf '%-18s %s/%s\n' "$slot" "$candidate" "$socket"
    done <<< "$discovered"
    exit 0
fi
if [ -n "$discovered" ]; then
    IFS=$'\t' read -r slot runtime_dir display <<< "$discovered"
fi

if [ -z "$runtime_dir" ] || [ -z "$display" ]; then
    echo "Error: Could not find a live Cocoa-Way socket for the current user." >&2
    echo "Start Cocoa-Way first, or set COCOA_WAY_RUNTIME_DIR explicitly." >&2
    exit 1
fi

if [ "$#" -eq 0 ]; then
    usage
    exit 2
fi

waypipe_bin="${COCOA_WAY_WAYPIPE:-$(command -v waypipe || true)}"
if [ -z "$waypipe_bin" ]; then
    echo "Error: waypipe is not installed on this Mac or is not available in PATH." >&2
    echo "Install it with: brew install J-x-Z/tap/waypipe-darwin" >&2
    exit 1
fi

export XDG_RUNTIME_DIR="$runtime_dir"
export WAYLAND_DISPLAY="$display"

echo "Found Cocoa-Way socket: $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
if [ -n "$selected_slot" ]; then
    echo "Using Cocoa-Way display slot: $selected_slot"
fi
echo "Using waypipe: $waypipe_bin"

# If the command is 'ssh', automatically add the recommended StreamLocalBindUnlink=yes option
# This fixes "remote port forwarding failed" errors when the socket file already exists on the remote
if [ "$1" = "ssh" ]; then
    echo "Info: Detected SSH mode. Injecting '-o StreamLocalBindUnlink=yes' to fix socket conflicts."
    echo "Info: The remote Linux host must also have a compatible 'waypipe' command in PATH."
    shift
    exec "$waypipe_bin" --compress="${COCOA_WAY_COMPRESS:-zstd}" ssh \
        -o StreamLocalBindUnlink=yes "$@"
else
    exec "$waypipe_bin" "$@"
fi

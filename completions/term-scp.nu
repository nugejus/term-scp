# nushell completion for term-scp, including remote folders over ssh.
# Install: add this line to your config.nu ($nu.config-path)
#   source /path/to/term-scp/completions/term-scp.nu

# Subfolders of the folder part of `path` on `host` (relative to the remote home; `~/` is the
# home), each with that folder part and a trailing '/'. `opts` go to ssh before the host.
def "nu-complete term-scp remote-dirs" [host: string, path: string, opts: list<string>] {
    let dir = if ($path | str contains "/") { $path | str replace -r '[^/]*$' '' } else { "" }
    # hidden folders only once a '.' is typed
    let globs = if ($path | str replace -r '^.*/' '' | str starts-with ".") { ".[!.]* ..?*" } else { "*" }
    let quote = {|s| "'" + ($s | str replace -a "'" "'\\''") + "'" }
    let arg = if $dir == "" { "." } else if ($dir | str starts-with "~/") {
        "~/" + (do $quote ($dir | str substring 2..))
    } else { do $quote $dir }
    let cmd = $"cd ($arg) 2>/dev/null && for x in ($globs); do [ -d \"$x\" ] && printf '%s\\n' \"$x\"; done"
    # BatchMode: never prompt; ConnectTimeout: give up in ~3 s when the host is unreachable
    let r = (do { ^ssh -o BatchMode=yes -o ConnectTimeout=3 ...$opts $host $cmd } | complete)
    $r.stdout | lines | where {|n| $n != "" } | each {|n| $"($dir)($n)/" }
    | where {|c| $c | str starts-with $path }
}

# Host names from ~/.ssh/config, without patterns. $env.HOME: $nu is not set in every
# completion context.
def "nu-complete term-scp hosts" [] {
    let config = ($env.HOME | path join ".ssh" "config")
    if not ($config | path exists) { return [] }
    open --raw $config | lines | each {|l| $l | str trim | split row -r '\s+' }
    | where {|w| ($w | length) > 1 and $w.0 =~ '(?i)^host$' }
    | each {|w| $w | skip 1 } | flatten
    | where {|h| not ($h =~ '[*?!]') }
}

# HOST: from ~/.ssh/config, then the folders on that host after the ':'.
def "nu-complete term-scp target" [context: string] {
    # The word under the cursor (a quoted one may hold spaces), without its quotes.
    let cur = ($context | parse -r '(?:^|\s)(?<w>(?:`[^`]*`?|"[^"]*"?|\x27[^\x27]*\x27?|[^\s`"\x27])*)$'
        | get -o w.0 | default "" | str replace -a -r '[`"\x27]' '')
    # The words before it: HOST:DIRs so far (an option's value is not one) and the ssh options.
    let words = ($context | str substring 0..<(($context | str length) - ($context | parse -r '(?<w>\S*)$' | get 0.w | str length))
        | split row -r '\s+' | where {|w| $w != "" } | skip 1)
    mut i = 0
    mut targets = 0
    mut has_l = false
    mut opts = []
    while $i < ($words | length) {
        let w = ($words | get $i)
        let next = ($words | get -o ($i + 1) | default "")
        match $w {
            "-l" | "--local" => { $has_l = true; $i += 1 }
            "-P" | "--port" => { $opts = ($opts | append ["-p" $next]); $i += 1 }
            "-i" | "--identity" => { $opts = ($opts | append ["-i" $next]); $i += 1 }
            "-o" | "--ssh-option" => { $opts = ($opts | append ["-o" $next]); $i += 1 }
            $x if ($x | str starts-with "--local=") => { $has_l = true }
            $x if not ($x | str starts-with "-") => { $targets += 1 }
            _ => {}
        }
        $i += 1
    }
    # At most two HOST:DIRs, and -l leaves room for one.
    if $targets >= 2 or ($has_l and $targets >= 1) {
        return { options: { sort: false }, completions: [] }
    }
    let found = if ($cur | str contains ":") {
        let host = ($cur | split row ":" | first)
        let path = ($cur | str replace -r '^[^:]*:' '')
        nu-complete term-scp remote-dirs $host $path $opts | each {|d| $"($host):($d)" }
    } else {
        nu-complete term-scp hosts | where {|h| $h | str starts-with $cur } | each {|h| $"($h):" }
    }
    # Quoted when the shell would split or expand it, as nushell quotes file names.
    let quoted = ($found | each {|v| if ($v =~ '[\s\x27"$#;|()\[\]{}]') { $"`($v)`" } else { $v } })
    { options: { sort: false }, completions: $quoted }
}

export extern "term-scp" [
    target?: string@"nu-complete term-scp target"   # [user@]host:[path]
    target2?: string@"nu-complete term-scp target"  # A second host: two hosts side by side, no local pane
    --port(-P): int                # ssh port
    --identity(-i): path           # ssh identity file
    --ssh-option(-o): string       # Extra ssh option (repeatable)
    --local(-l): directory         # Folder the local pane starts in (one host only)
    --no-icons                     # No file icons
    --help(-h)                     # Print help
    --version(-V)                  # Print version
]

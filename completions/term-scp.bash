# bash completion for term-scp, including remote folders over ssh. Works on bash 3.2+.
# Install: add this line to ~/.bashrc
#   source /path/to/term-scp/completions/term-scp.bash

# Host names from ~/.ssh/config, without patterns.
_term_scp_hosts() {
    local key rest h
    local -a hs
    [[ -r ~/.ssh/config ]] || return 0
    while read -r key rest; do
        case $key in [Hh][Oo][Ss][Tt]) ;; *) continue ;; esac
        read -r -a hs <<< "$rest" # no glob expansion, unlike a bare `for h in $rest`
        for h in "${hs[@]}"; do
            case $h in *[*?!]*) ;; *) printf '%s\n' "$h" ;; esac
        done
    done < ~/.ssh/config
}

# Subfolders of the folder part of $2 on host $1 (relative to the remote home; `~/` is the
# home), each printed with that folder part and a trailing '/'. The rest are ssh options.
_term_scp_remote() {
    local host=$1 path=$2 dir= arg globs='*'
    shift 2
    [[ $path == */* ]] && dir=${path%/*}/
    # hidden folders only once a '.' is typed
    [[ ${path##*/} == .* ]] && globs='.[!.]* ..?*'
    case $dir in
        '') arg=. ;;
        '~/'*) arg="~/'$(printf '%s' "${dir#'~/'}" | sed "s/'/'\\\\''/g")'" ;;
        *) arg="'$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")'" ;;
    esac
    # BatchMode: never prompt; ConnectTimeout: give up in ~3 s when the host is unreachable
    ssh -o BatchMode=yes -o ConnectTimeout=3 "$@" "$host" \
        "cd $arg 2>/dev/null && for x in $globs; do [ -d \"\$x\" ] && printf '%s\\n' \"\$x\"; done" \
        < /dev/null 2> /dev/null | while IFS= read -r x; do printf '%s\n' "$dir$x/"; done
}

# Local paths matching $1: folders with a trailing '/', and files too unless $2 is `dirs`.
_term_scp_local() {
    local p
    compgen -f -- "$1" | while IFS= read -r p; do
        if [[ -d ${p/#\~/$HOME} ]]; then
            printf '%s\n' "$p/"
        elif [[ $2 != dirs ]]; then
            printf '%s\n' "$p"
        fi
    done
}

_term_scp() {
    # Words come from COMP_LINE, not COMP_WORDS: bash splits COMP_WORDS at ':' and '='.
    # `read` without -r undoes backslash escapes, the quoting the candidates below get.
    local line=${COMP_LINE:0:COMP_POINT} cur= prev= opt= val w c i n=0 targets=0 has_l=
    local -a words cands=() sshopt=()
    read -a words <<< "$line"
    if [[ ! ($line == *[[:space:]] && $line != *\\[[:space:]]) && ${#words[@]} -gt 0 ]]; then
        cur=${words[${#words[@]}-1]}
        unset "words[${#words[@]}-1]"
    fi
    [[ ${#words[@]} -gt 0 ]] && prev=${words[${#words[@]}-1]}
    # HOST:DIRs given so far (an option's value is not one), and the ssh options to list with.
    for ((i = 1; i < ${#words[@]}; i++)); do
        w=${words[i]}
        case $w in
            -l | --local) has_l=1 i=$((i + 1)) ;;
            -P | --port) sshopt+=(-p "${words[i+1]}") i=$((i + 1)) ;;
            -i | --identity) sshopt+=(-i "${words[i+1]}") i=$((i + 1)) ;;
            -o | --ssh-option) sshopt+=(-o "${words[i+1]}") i=$((i + 1)) ;;
            --port=*) sshopt+=(-p "${w#*=}") ;;
            --identity=*) sshopt+=(-i "${w#*=}") ;;
            --ssh-option=*) sshopt+=(-o "${w#*=}") ;;
            -l* | --local=*) has_l=1 ;;
            -*) ;;
            *) targets=$((targets + 1)) ;;
        esac
    done
    val=$cur
    case $cur in
        --local=* | --identity=* | --port=* | --ssh-option=*) opt=${cur%%=*} val=${cur#*=} ;;
        -*) ;;
        *) case $prev in -l | --local | -i | --identity | -P | --port | -o | --ssh-option) opt=$prev ;; esac ;;
    esac
    case $opt in
        -l | --local)
            while IFS= read -r c; do cands+=("$c"); done < <(_term_scp_local "$val" dirs)
            ;;
        -i | --identity)
            while IFS= read -r c; do cands+=("$c"); done < <(_term_scp_local "$val")
            ;;
        -P | --port | -o | --ssh-option) ;;
        *)
            if [[ $cur == -* ]]; then
                # the trailing space: `-o nospace` holds back the one bash would add
                for c in --port --identity --ssh-option --local --no-icons --help --version; do
                    [[ $c == --local && $targets -ge 2 ]] && continue # -l and a second host exclude each other
                    [[ $c == "$cur"* ]] && cands+=("$c ")
                done
            elif [[ $targets -lt 2 && ! ($targets -eq 1 && -n $has_l) ]]; then
                if [[ $val == *:* ]]; then
                    while IFS= read -r c; do
                        [[ $c == "${val#*:}"* ]] && cands+=("${val%%:*}:$c")
                    done < <(_term_scp_remote "${val%%:*}" "${val#*:}" "${sshopt[@]}")
                else
                    while IFS= read -r c; do
                        [[ $c == "$val"* ]] && cands+=("$c:")
                    done < <(_term_scp_hosts)
                fi
            fi
            ;;
    esac
    [[ $cur == --*=* && -n $opt ]] && for i in "${!cands[@]}"; do
        cands[i]="${cur%%=*}=${cands[i]}"
    done
    # bash replaces only the text after the last ':' or '=' it split at: drop what comes before
    for ((i = 0; i < ${#cur}; i++)); do
        c=${cur:i:1}
        [[ ($c == : || $c == =) && $COMP_WORDBREAKS == *"$c"* ]] && n=$((i + 1))
    done
    COMPREPLY=()
    for c in "${cands[@]}"; do
        c=${c:n}
        # Quoted for the shell (spaces, quotes, $); a leading ~ stays bare so the shell expands it.
        # A flag keeps its trailing space unquoted.
        case $c in
            *' ') COMPREPLY+=("$c") ;;
            '~'*) COMPREPLY+=("~$(printf %q "${c#\~}")") ;;
            *) COMPREPLY+=("$(printf %q "$c")") ;;
        esac
    done
}

complete -o nospace -F _term_scp term-scp

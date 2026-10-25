# Markdown hygiene filter of the change-aware verification.
#
#   awk -f scripts/mdcheck.awk <markdown path>...
#
# The dispatcher `scripts/verify.sh` supplies the markdown paths of the turn's
# change set. Findings are printed on standard output as
# `path:line: error: message`, the shape the harness parses into typed
# diagnostics, and a non-zero exit status reports that at least one was found.
#
# Two rules are enforced. Both are deliberately narrow, because a false positive
# fails the verification gate and burns a healing round:
#
#   1. A leftover turn-marker protocol tag outside a fenced code block. The
#      marker is the 16-character hexadecimal suffix that `generate_turn_marker`
#      emits through `format!("{:016X}")` in
#      `crates/core/src/edits/protocol/xml/marker.rs`, so a plain documented tag
#      name never matches while a leaked marker-suffixed tag always does.
#   2. A fenced code block left open, reported on the line that opened it.
#
# Fences follow CommonMark closely enough for documentation: a line whose first
# non-blank characters are three or more backticks or tildes opens a block, and
# only a run of the same delimiter, at least as long and followed by nothing but
# whitespace, closes it.

BEGIN {
    fence = ""
    fence_line = 0
    fence_file = ""
    findings = 0

    # Without a path there is nothing to examine, and reading standard input
    # here would consume the change set the dispatcher already parsed.
    if (ARGC < 2) {
        print "verify-md: the change set carries no markdown paths" > "/dev/stderr"
        exit 0
    }
}

# A block left open by the previous file is a finding of that file.
FNR == 1 {
    report_open_fence()
}

{
    boundary = 0
    run = fence_run($0)

    if (fence == "") {
        if (run != "") {
            fence = run
            fence_line = FNR
            fence_file = FILENAME
            boundary = 1
        }
    } else if (run != "" && closes_fence($0, fence)) {
        fence = ""
        fence_line = 0
        fence_file = ""
        boundary = 1
    }

    if (fence == "" && boundary == 0) {
        report_marker_tags()
    }
}

END {
    report_open_fence()
    exit (findings > 0 ? 1 : 0)
}

# Reports a fenced code block that the previous file left open.
function report_open_fence() {
    if (fence == "") {
        return
    }
    report(fence_file, fence_line, "unterminated fenced code block")
    fence = ""
    fence_line = 0
    fence_file = ""
}

# Reports every leftover turn-marker protocol tag on the current line.
function report_marker_tags(   rest, tag) {
    rest = $0
    while (match(rest, /<\/?[A-Za-z][A-Za-z0-9_-]*_[0-9A-Fa-f]{16}>/)) {
        tag = substr(rest, RSTART, RLENGTH)
        report(FILENAME, FNR, "leftover turn-marker protocol tag " tag)
        rest = substr(rest, RSTART + RLENGTH)
    }
}

# Emits one finding in the diagnostic shape the harness parses.
function report(file, line, message) {
    printf "%s:%d: error: %s\n", file, line, message
    findings++
}

# Returns the delimiter run that starts the first non-blank character of the
# line, or the empty string when the line does not start a fence.
function fence_run(line,   text, delimiter, count, i, run) {
    text = line
    sub(/^[ \t]+/, "", text)
    if (text == "") {
        return ""
    }
    delimiter = substr(text, 1, 1)
    if (delimiter != "`" && delimiter != "~") {
        return ""
    }
    count = 0
    for (i = 1; i <= length(text); i++) {
        if (substr(text, i, 1) != delimiter) {
            break
        }
        count++
    }
    if (count < 3) {
        return ""
    }
    run = ""
    for (i = 1; i <= count; i++) {
        run = run delimiter
    }
    return run
}

# Reports whether the line closes the block opened by `open`: the same delimiter,
# a run at least as long, and nothing but whitespace after it.
function closes_fence(line, open,   text, run, rest) {
    run = fence_run(line)
    if (run == "") {
        return 0
    }
    if (substr(run, 1, 1) != substr(open, 1, 1)) {
        return 0
    }
    if (length(run) < length(open)) {
        return 0
    }
    text = line
    sub(/^[ \t]+/, "", text)
    rest = substr(text, length(run) + 1)
    return rest !~ /[^ \t]/
}

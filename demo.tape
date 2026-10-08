# vhs tape for the README demo. Opens this repository's own PRs #1 and #2.
# Regenerate with: cargo build --release && vhs demo.tape
# Every Viewed toggle below is undone later in the tape, so re-recording
# leaves the PRs as they were.
Output demo.gif

Set FontSize 15
Set Width 1400
Set Height 820
Set Padding 16
Set Theme "TokyoNight"
Set TypingSpeed 40ms
Set Shell "bash"

Hide
Type 'export PATH="$PWD/target/release:$PATH" && clear'
Enter
Type "laugh pr azihsoyn/laugh#1 azihsoyn/laugh#2"
Enter
Sleep 5s
Show

# Two PRs, one tree. ⟳ = changed since you viewed it.
Sleep 2.5s
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 1.2s
Type "j"
Sleep 300ms
Type "j"
Sleep 600ms

# v: one file viewed, and back.
Type "v"
Sleep 1.4s
Type "v"
Sleep 900ms

# V on a PR's root: the whole PR at once. H hides what's done.
Type "G"
Sleep 400ms
Type "k"
Sleep 250ms
Type "k"
Sleep 800ms
Type "V"
Sleep 1.5s
Type "H"
Sleep 2s
Type "H"
Sleep 1s
Type "G"
Sleep 300ms
Type "k"
Sleep 250ms
Type "k"
Sleep 400ms
Type "V"
Sleep 1s

# One PR at a time, then back to All.
Type "]"
Sleep 1.6s
Type "]"
Sleep 1.4s
Type "["
Sleep 300ms
Type "["
Sleep 1s

# Every review thread, resolved ones included, with the code it's about.
Type "2"
Sleep 2.5s
Type "l"
Sleep 1.3s
Type "l"
Sleep 1.3s
Type "r"
Sleep 1.8s
Type "r"
Sleep 1s
Type "h"
Sleep 300ms
Type "h"
Sleep 800ms
Type " "
Sleep 400ms
Type "j"
Sleep 150ms
Type "j"
Sleep 150ms
Type "j"
Sleep 1.2s

Type "?"
Sleep 3s
Escape
Sleep 800ms
Type "q"
Sleep 500ms

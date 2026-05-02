# vhs script for the README demo GIF.
#
# Install vhs: https://github.com/charmbracelet/vhs
# Render:      vhs demo.tape
#
# Renders to demo.gif. The script assumes:
#   - `claws` is on PATH and starts cleanly (no leftover sessions)
#   - `claude` is on PATH (claws spawns it as a subprocess)
#   - the terminal is wide enough for grid mode (>= 80 cols handled by vhs)
#
# If you want a clean recording, kill the daemon first:
#   claws kill-server

Output demo.gif

Set FontSize 14
Set Width 1280
Set Height 720
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

# Empty-state landing — let the wordmark breathe.
Type "claws"
Enter
Sleep 2s

# Spawn first session in the current dir.
Type "c"
Sleep 800ms
Tab
Type " --dangerously-skip-permissions"
Sleep 400ms
Enter
Sleep 1500ms

# Spawn a second from the same form.
Type "c"
Sleep 600ms
Enter
Sleep 1200ms

# Spawn a third.
Type "c"
Sleep 400ms
Enter
Sleep 1500ms

# Flip to grid view — wall of live cards.
Type "g"
Sleep 2500ms

# Theme picker — live preview as we arrow through.
Type "t"
Sleep 800ms
Down
Sleep 700ms
Down
Sleep 700ms
Down
Sleep 700ms
Up
Sleep 600ms
Enter
Sleep 1200ms

# Back to sidebar+detail layout.
Type "g"
Sleep 1200ms

# Attach into the selected session.
Enter
Sleep 2s

# Detach back to the dashboard via prefix.
Ctrl+Space
Type "d"
Sleep 1500ms

# Pop the help modal as a closing beat.
Type "?"
Sleep 2500ms
Escape
Sleep 600ms

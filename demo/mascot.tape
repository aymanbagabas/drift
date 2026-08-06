# drift idle mascot — render with:  vhs demo/mascot.tape
# The 👾 alien appears on the idle screen when the working tree is clean and
# drifts around on its own.
Output demo/mascot.gif

Set Shell "bash"
Set FontSize 16
Set Width 900
Set Height 560
Set Padding 20
Set Theme "Dracula"

Hide
Type 'alias drift="$PWD/target/release/drift"' Enter
Type "rm -rf /tmp/drift-clean && mkdir /tmp/drift-clean && cd /tmp/drift-clean" Enter
Type "git init -q && git commit -q --allow-empty -m init" Enter
Type "clear" Enter
Show

Type "drift" Enter
# let the alien drift and cycle through its idle expressions
Sleep 9s

Hide
Type "q"
Sleep 500ms

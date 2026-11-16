# Records demo/demo.gif. Needs VHS (https://github.com/charmbracelet/vhs):
#
#   cargo build --release && vhs demo/demo.tape
#
Output demo/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1300
Set Height 620
Set Padding 18
Set TypingSpeed 45ms

Hide
Type "export PATH=$PWD/target/release:$PATH PS1='$ ' && set +m && . demo/setup.sh 2>/dev/null && clear"
Enter
Sleep 2s
Show

Type "wits"
Sleep 400ms
Enter
Sleep 6s

Type "wits deploy"
Sleep 400ms
Enter
Sleep 6s

Type "wits backup"
Sleep 400ms
Enter
Sleep 6s

Type "wits report"
Sleep 400ms
Enter
Sleep 6s

Hide
Type "demo_stop"
Enter
Sleep 500ms

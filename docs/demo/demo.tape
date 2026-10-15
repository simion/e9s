# The README recording: `make demo` (needs vhs). E9S_DEMO=1 answers every read from a made-up account.
Output docs/demo.gif
Set Shell bash
Set FontSize 14
Set Width 1400
Set Height 820
Set Padding 16
Set TypingSpeed 60ms
Set Theme "Builtin Dark"

Hide
Type "export E9S_DEMO=1 AWS_CONFIG_FILE=docs/demo/aws-config AWS_SHARED_CREDENTIALS_FILE=/dev/null PATH=$PWD/target/release:$PATH; unset AWS_PROFILE; export PS1='$ '; clear"
Enter
Sleep 500ms
Show

Type "e9s -p prod -r us-east-1"
Sleep 400ms
Enter
Sleep 2.5s
Enter
Sleep 3s
Type "R"
Sleep 4s
Escape
Sleep 800ms
Type "l"
Sleep 6s
Escape
Sleep 800ms
Type "m"
Sleep 3.5s
Escape
Sleep 800ms
Down 4
Sleep 700ms
Type "F"
Sleep 4s
Escape
Sleep 800ms
Up 4
Enter
Sleep 2.5s
Enter
Sleep 3s
Type "5"
Sleep 2s
Enter
Sleep 1.5s
Enter
Sleep 3s
Type ":sqs"
Sleep 500ms
Enter
Sleep 3s
Type "H"
Sleep 4s

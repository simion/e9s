# The README recording: `make demo` (needs vhs). E9S_DEMO=1 answers every read from a made-up account;
# HOME is docs/demo/home, so the profiles and their SSO login are made up too.
# 1750x796 at FontSize 22 is 120x30, the smallest terminal with the full header and logo.
Output docs/demo.gif
Set Shell bash
Set FontSize 22
Set Width 1750
Set Height 796
Set Padding 16
Set TypingSpeed 80ms
Set Theme "Builtin Dark"

Hide
Type "export E9S_DEMO=1 HOME=$PWD/docs/demo/home PATH=$PWD/target/release:$PATH; unset AWS_PROFILE AWS_CONFIG_FILE AWS_SHARED_CREDENTIALS_FILE AWS_REGION AWS_DEFAULT_REGION; export PS1='$ '; clear"
Enter
Sleep 500ms
Show

# Profiles, then the region scan, clusters and services.
Type "e9s"
Sleep 500ms
Enter
Sleep 2.5s
Enter
Sleep 2.5s
Enter
Sleep 2.5s
Enter
Sleep 3s

# A rollout, live logs and metrics of the first service.
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

# The service that keeps running out of memory.
Down 4
Sleep 700ms
Type "F"
Sleep 4s
Escape
Sleep 800ms

# Tasks of the rolling service, both generations.
Up 4
Enter
Sleep 3s

# Load balancer targets, SQS queues, and the AWS calls behind the screen.
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

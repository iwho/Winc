#!/bin/sh
set -eu
test "$1" = app-server
read -r initialize
case "$initialize" in *'"method":"initialize"'*) ;; *) exit 1;; esac
printf '%s\n' '{"id":0,"result":{"userAgent":"mock"}}'
read -r initialized
test "$initialized" = '{"method":"initialized"}'
read -r account
case "$account" in *'"method":"account/read"'*) ;; *) exit 1;; esac
printf '%s\n' '{"id":1,"result":{"requiresOpenaiAuth":false}}'
read -r start
case "$start" in *'"sandbox":"workspace-write"'*|*'"approvalPolicy":"on-request"'*) ;; *) exit 1;; esac
printf '{"id":2,"result":{"thread":{"id":"s","cwd":"%s"}}}\n' "$PWD"
read -r prompt
case "$prompt" in *'"text":"你好"'*) ;; *) exit 1;; esac
printf '%s\n' \
 '{"id":3,"result":{"turn":{"id":"t","status":"inProgress","items":[]}}}' \
 '{"method":"turn/started","params":{"threadId":"s","turn":{"id":"t","status":"inProgress","items":[]}}}' \
 '{"method":"future/event","params":{"new":true}}' \
 '{"method":"item/agentMessage/delta","params":{"threadId":"s","turnId":"t","itemId":"i","delta":"你"}}' \
 '{"method":"item/agentMessage/delta","params":{"threadId":"s","turnId":"t","itemId":"i","delta":"好"}}' \
 '{"id":"a","method":"item/commandExecution/requestApproval","params":{"threadId":"s","turnId":"t","itemId":"command","command":"echo test"}}'
read -r answer
test "$answer" = '{"id":"a","result":{"decision":"decline"}}'
printf '%s\n' \
 '{"id":"file","method":"item/fileChange/requestApproval","params":{"threadId":"s","turnId":"t","itemId":"file","reason":"test patch"}}' \
 '{"method":"item/fileChange/patchUpdated","params":{"threadId":"s","turnId":"t","itemId":"file","changes":[{"path":"result.txt","diff":"+ok"}]}}'
read -r file_answer
test "$file_answer" = '{"id":"file","result":{"decision":"accept"}}'
read -r stop
case "$stop" in *'"method":"turn/interrupt"'*) ;; *) exit 1;; esac
printf '%s\n' \
 '{"id":6,"result":{}}' \
 '{"method":"turn/completed","params":{"threadId":"s","turn":{"id":"t","status":"interrupted","items":[]}}}'
read -r shutdown || true

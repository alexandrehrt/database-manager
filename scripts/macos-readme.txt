Installing Cuia
===============

1. Drag Cuia into the Applications folder next to it.

2. Open Cuia from Applications (or Spotlight).

   The first time, macOS may say it can't verify the developer, because this
   copy isn't signed with an Apple Developer ID. To allow it, once:

   - Open System Settings > Privacy & Security, scroll down to the message
     about Cuia, click "Open Anyway" and confirm with your password.

   Or, in Terminal:

       xattr -dr com.apple.quarantine /Applications/Cuia.app

   After that it opens normally.

Cuia runs on Apple Silicon and Intel Macs with macOS 11 (Big Sur) or later.
Oracle connections need Oracle Instant Client installed; PostgreSQL and SQLite
need nothing else.

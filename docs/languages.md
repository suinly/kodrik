# Interface languages

Kodrik supports English (`en`) and Russian (`ru`) in the web interface, terminal
UI and Telegram. Other detected languages fall back to English.

## Web

The language icon opens a menu with **System**, **English** and **Русский**.
System is the default and follows the browser's preferred language, including
changes while the page is open. An explicit language overrides that preference.
The theme icon offers **System**, **Light** and **Dark**; System follows the
operating system's appearance. Both choices are saved in that browser.
Built-in controls, dates, numbers, status messages and errors
follow the selected language. User messages, saved conversation titles and
published document content retain their original language.

## Terminal

The TUI detects the terminal language from `LC_ALL`, `LC_MESSAGES`, then `LANG`.
Use `/language en` or `/language ru` to change it without invoking the model.
The explicit choice persists across launches and new conversations.

## Telegram

Telegram's profile language selects the initial interface language. Send
`/language` and choose **English** or **Русский**, or use `/language en` or
`/language ru`, to save an explicit choice for the chat. The command and buttons
are handled locally and do not send a request to the model.

The selected language controls service messages and buttons. Agent replies
continue to follow the conversation's language and the user's instructions.
Telegram's native command descriptions follow the Telegram client's language.

## Data and installation

Configuration, history and saved interface languages live under `~/.kodrik`.
The macOS installer uses the `kodrik` binary, `com.kodrik.gateway` service and
`Kodrik Local` signing certificate.

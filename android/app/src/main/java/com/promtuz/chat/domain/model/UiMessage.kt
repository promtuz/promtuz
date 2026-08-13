package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable

/**
 * A message shaped for rendering. Keyed on [key] — the shared dispatch id when
 * present, else the local ULID — so edit / delete / reaction / receipt, which all
 * mutate the same row in place, surface as updates to the *same* list item rather
 * than churn. Built from libcore's MessageRecord joined with its reactions. Every
 * field is value-equality-friendly (no raw ByteArray) so LazyColumn diffing is
 * correct; the 16-byte dispatch id rides as hex and converts at the FFI boundary.
 */
@Immutable
data class UiMessage(
    val key: String,
    val localId: String,
    val dispatchIdHex: String?,
    val content: MessageContent,
    val outgoing: Boolean,
    /**
     * Who wrote this, as hex — null when it's us. In a group the bubble shows
     * it above the first message of each run; a 1:1 has one possible author,
     * so it stays hidden there.
     */
    val senderHex: String? = null,
    /** Resolved display name for [senderHex]; null when it's us or unknown. */
    val senderName: String? = null,
    val status: SendStatus,
    val edited: Boolean,
    val deleted: Boolean,
    val timestampMs: Long,
    val reactions: List<ReactionGroup>,
    /** The quoted message, when this is a reply. */
    val quote: Quote? = null,
    /**
     * How many other members have read this, for our own messages in a group.
     * 0 elsewhere: a 1:1 already says the same thing with its delivery tick,
     * and an incoming message's read state is not ours to report.
     */
    val seenBy: Int = 0,
)

/**
 * Quoted-message snippet, resolved at load from the quoted dispatch_id.
 * [text] is null when the quoted message isn't in the loaded window (or was
 * hard-deleted) — render a "message unavailable" shell.
 */
@Immutable
data class Quote(
    val dispatchIdHex: String,
    val text: String?,
    val outgoing: Boolean,
)

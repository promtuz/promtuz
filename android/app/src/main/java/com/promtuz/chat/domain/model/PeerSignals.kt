package com.promtuz.chat.domain.model

/**
 * Live peer activity bits, matching `common::proto::client_rel::ACTIVITY_*`.
 * Ephemeral — relay-routed, online-only, never stored — so the UI times each
 * signal out after a few seconds (an offline peer sends no "stopped").
 */
enum class Activity(val bit: Int) {
    Typing(1 shl 0),
    ChoosingSticker(1 shl 1), // covers emoji + sticker
    UploadingMedia(1 shl 2),
    UploadingDocument(1 shl 3),
    RecordingVoice(1 shl 4);

    companion object {
        fun fromBits(bits: Int): Set<Activity> = entries.filterTo(HashSet()) { bits and it.bit != 0 }
    }
}

/** A contact's presence (relay-derived; Idle is client-asserted). */
sealed interface Presence {
    data object Online : Presence
    /** Connected but backgrounded since [sinceMs] (unix ms). */
    data class Idle(val sinceMs: Long) : Presence
    /** Offline since [atMs] (unix ms). */
    data class LastSeen(val atMs: Long) : Presence
    /** Offline, time unknown. */
    data object Unknown : Presence
}

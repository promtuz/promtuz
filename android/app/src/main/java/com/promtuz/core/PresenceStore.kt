package com.promtuz.core

import android.content.Context
import android.content.SharedPreferences
import com.promtuz.chat.domain.model.Presence
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

@Serializable
private data class PresenceEntry(val kind: Int, val ts: Long)

@Serializable
private data class PresenceSnapshot(val savedAt: Long, val peers: Map<String, PresenceEntry>)

/** Persist observations separately from live reachability. Online never erases last seen. */
object PresenceStore {
    private lateinit var prefs: SharedPreferences
    private val json = Json { ignoreUnknownKeys = true }
    // SharedPreferences changes its memory even when commit() fails. Only successful writes
    // belong in this map, which is also the source for display and duplicate suppression.
    private val committed = mutableMapOf<String, Long>()

    @Synchronized
    fun init(context: Context) {
        prefs = context.getSharedPreferences("presence", Context.MODE_PRIVATE)
        // Migrate the old whole-map cache once without clearing other app/device data.
        prefs.getString("presence", null)?.let { encoded ->
            val snap = runCatching { json.decodeFromString<PresenceSnapshot>(encoded) }.getOrNull()
            val editor = prefs.edit().remove("presence")
            snap?.peers?.forEach { (peer, entry) ->
                if (!prefs.contains(peer) && entry.kind == 2 && entry.ts > 0) {
                    editor.putLong(peer, entry.ts)
                }
            }
            check(editor.commit()) { "Could not migrate presence cache" }
        }
        committed.clear()
        prefs.all.forEach { (peer, value) -> if (value is Long) committed[peer] = value }
    }

    @Synchronized
    fun seed(): Map<String, Presence> = committed.mapValues { restored(it.value) }

    @Synchronized
    fun lastSeen(peer: String): Presence = restored(committed[peer] ?: 0)

    /** Called by the serial IO worker before displaying the accepted observation. */
    @Synchronized
    fun record(peer: String, presence: Presence) {
        val timestamp = when (presence) {
            Presence.Online -> return
            is Presence.Idle -> presence.sinceMs
            is Presence.LastSeen -> presence.atMs
            Presence.Unknown -> 0L // Explicit withdrawal must also clear the retained observation.
        }
        if (committed[peer] == timestamp) return
        check(prefs.edit().putLong(peer, timestamp).commit()) { "Could not persist presence" }
        committed[peer] = timestamp
    }

    private fun restored(timestamp: Long): Presence =
        if (timestamp > 0) Presence.LastSeen(timestamp) else Presence.Unknown
}

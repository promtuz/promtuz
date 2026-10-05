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
    }

    fun seed(): Map<String, Presence> = prefs.all.mapNotNull { (peer, value) ->
        (value as? Long)?.let { peer to restored(it) }
    }.toMap()

    fun lastSeen(peer: String): Presence = restored(prefs.getLong(peer, 0))

    /** Called by the serial IO worker before displaying the accepted observation. */
    @Synchronized
    fun record(peer: String, presence: Presence) {
        val timestamp = when (presence) {
            Presence.Online -> return
            is Presence.Idle -> presence.sinceMs
            is Presence.LastSeen -> presence.atMs
            Presence.Unknown -> 0L // Explicit withdrawal must also clear the retained observation.
        }
        if (prefs.getLong(peer, -1) == timestamp) return
        check(prefs.edit().putLong(peer, timestamp).commit()) { "Could not persist presence" }
    }

    private fun restored(timestamp: Long): Presence =
        if (timestamp > 0) Presence.LastSeen(timestamp) else Presence.Unknown
}

package com.promtuz.core

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import com.promtuz.chat.domain.model.Presence
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

@Serializable
private data class PresenceEntry(val kind: Int, val ts: Long)

@Serializable
private data class PresenceSnapshot(val savedAt: Long, val peers: Map<String, PresenceEntry>)

/**
 * Persists the last-known presence per contact (hex IPK) so a cold app start
 * shows relay-reported last-seens immediately. Live states cannot be trusted
 * across restart, so [seed] drops them until a fresh relay snapshot confirms
 * their current state. One JSON blob in prefs; written off the hot path.
 */
object PresenceStore {
    private const val KEY = "presence"
    private lateinit var prefs: SharedPreferences
    private val json = Json { ignoreUnknownKeys = true }

    fun init(context: Context) {
        prefs = context.getSharedPreferences("presence", Context.MODE_PRIVATE)
    }

    /** Cold-start seed: only persisted relay-reported offline states are valid. */
    fun seed(): Map<String, Presence> {
        val snap = prefs.getString(KEY, null)
            ?.let { runCatching { json.decodeFromString<PresenceSnapshot>(it) }.getOrNull() }
            ?: return emptyMap()
        return snap.peers.mapValues { (_, e) -> restore(e) }
    }

    fun save(map: Map<String, Presence>, savedAt: Long) {
        val peers = map.mapValues { (_, p) -> entry(p) }
        prefs.edit { putString(KEY, json.encodeToString(PresenceSnapshot.serializer(), PresenceSnapshot(savedAt, peers))) }
    }

    private fun entry(p: Presence): PresenceEntry = when (p) {
        Presence.Online -> PresenceEntry(0, 0)
        is Presence.Idle -> PresenceEntry(1, p.sinceMs)
        is Presence.LastSeen -> PresenceEntry(2, p.atMs)
        Presence.Unknown -> PresenceEntry(3, 0)
    }

    // A cached live state cannot prove when a peer was last online.
    private fun restore(e: PresenceEntry): Presence = when (e.kind) {
        0, 1 -> Presence.Unknown
        2 -> Presence.LastSeen(e.ts)
        else -> Presence.Unknown
    }
}

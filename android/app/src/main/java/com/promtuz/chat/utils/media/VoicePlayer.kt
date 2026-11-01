package com.promtuz.chat.utils.media

import android.content.Context
import android.media.AudioAttributes
import android.media.MediaPlayer
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import android.media.MediaDataSource

/** Playback reads memory directly, so private audio never spills to a plaintext cache file. */
object VoicePlayer {
    data class Playback(val dispatchIdHex: String, val positionMs: Int, val playing: Boolean)

    private val _state = MutableStateFlow<Playback?>(null)
    val state: StateFlow<Playback?> = _state.asStateFlow()

    private var player: MediaPlayer? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var ticker: Job? = null

    fun toggle(context: Context, dispatchIdHex: String, bytes: ByteArray, mime: String) {
        val current = _state.value
        val p = player
        if (p != null && current?.dispatchIdHex == dispatchIdHex) {
            if (p.isPlaying) {
                p.pause()
                _state.value = current.copy(positionMs = p.currentPosition, playing = false)
                ticker?.cancel()
            } else {
                p.start()
                _state.value = current.copy(playing = true)
                tick()
            }
            return
        }
        stop()
        val privateConversation = com.promtuz.chat.security.PrivateChatSession.active.value
        val privateSession = com.promtuz.chat.security.PrivateChatSession.sessionId
        val source = object : MediaDataSource() {
            private var data = bytes.copyOf()
            @Synchronized override fun getSize(): Long = data.size.toLong()
            @Synchronized override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
                if (privateConversation != null && (com.promtuz.chat.security.PrivateChatSession.active.value != privateConversation ||
                    com.promtuz.chat.security.PrivateChatSession.sessionId != privateSession)) {
                    data.fill(0); data = byteArrayOf(); return -1
                }
                if (position < 0 || position >= data.size) return -1
                val count = minOf(size, data.size - position.toInt())
                data.copyInto(buffer, offset, position.toInt(), position.toInt() + count)
                return count
            }
            @Synchronized override fun close() { data.fill(0); data = byteArrayOf() }
        }
        val mp = MediaPlayer().apply {
            setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build(),
            )
            setOnCompletionListener { stop() }
            setOnErrorListener { _, _, _ -> stop(); true }
        }
        runCatching {
            mp.setDataSource(source)
            mp.prepare()
            mp.start()
        }.onFailure {
            mp.release()
            source.close()
            stop()
            return
        }
        audioSource = source
        player = mp
        _state.value = Playback(dispatchIdHex, 0, true)
        tick()
    }

    /** Remove only the two obsolete playback scratch names from older app versions. */
    fun clearLegacyScratch(context: Context) {
        listOf("playing.m4a", "playing.ogg").forEach {
            java.io.File(java.io.File(context.cacheDir, "voice"), it).delete()
        }
    }

    fun stop() {
        ticker?.cancel()
        player?.let { runCatching { it.stop() }; it.release() }
        player = null
        audioSource?.close()
        audioSource = null
        _state.value = null
    }

    private var audioSource: MediaDataSource? = null

    private fun tick() {
        ticker?.cancel()
        ticker = scope.launch {
            while (true) {
                delay(100)
                val p = player ?: break
                val s = _state.value ?: break
                _state.value = s.copy(positionMs = p.currentPosition, playing = p.isPlaying)
            }
        }
    }

}

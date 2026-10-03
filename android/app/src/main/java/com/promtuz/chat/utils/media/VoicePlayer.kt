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
import java.io.File

/** MediaPlayer wants a file, so the note spills to one scratch file that [stop] unlinks; the database keeps the only copy. */
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
        val file = spill(context, bytes, mime)
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
            mp.setDataSource(file.absolutePath)
            mp.prepare()
            mp.start()
        }.onFailure {
            mp.release()
            stop()
            return
        }
        player = mp
        _state.value = Playback(dispatchIdHex, 0, true)
        tick()
    }

    fun stop() {
        ticker?.cancel()
        player?.let { runCatching { it.stop() }; it.release() }
        player = null
        scratch?.delete()
        scratch = null
        _state.value = null
    }

    private var scratch: File? = null

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

    private fun spill(context: Context, bytes: ByteArray, mime: String): File {
        val dir = File(context.cacheDir, "voice").apply { mkdirs() }
        val ext = if (mime == "audio/mp4") "m4a" else "ogg"
        val f = File(dir, "playing.$ext")
        f.writeBytes(bytes)
        scratch = f
        return f
    }
}

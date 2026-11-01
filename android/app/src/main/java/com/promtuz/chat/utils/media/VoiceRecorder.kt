package com.promtuz.chat.utils.media

import android.content.Context
import android.media.MediaRecorder
import android.os.Build
import android.os.SystemClock
import java.io.File

/** Opus in Ogg from API 29, AAC in MP4 below, and the mime rides the message. The file-size cap keeps a
 *  note under core's inline budget, so a long note stops itself rather than failing to send. */
class VoiceRecorder(private val context: Context) {
    private var recorder: MediaRecorder? = null
    private var file: File? = null
    private var privateCapture: PrivateMediaAccess.Capture? = null
    private var outputDescriptor: android.os.ParcelFileDescriptor? = null
    private var startedAt = 0L
    private val samples = ArrayList<Int>()
    val mime: String get() = if (opus) "audio/ogg" else "audio/mp4"
    private val opus = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

    val isRecording: Boolean get() = recorder != null

    fun start(privateConversation: String? = null, onLimit: () -> Unit): Boolean {
        if (recorder != null) return true
        val dir = File(context.cacheDir, "voice").apply { mkdirs() }
        val out = if (privateConversation == null) File(dir, "rec_${System.nanoTime()}.${if (opus) "ogg" else "m4a"}") else null
        val capture = try { privateConversation?.let { PrivateMediaAccess.capture(context, it, "voice", mime,
            nativeCodec = true, maxBytes = MAX_BYTES + 64 * 1024) } }
            catch (_: Exception) { return false }
        val descriptor = try { capture?.descriptor(context) } catch (_: Exception) { capture?.close(); return false }
        val r = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) MediaRecorder(context)
                else @Suppress("DEPRECATION") MediaRecorder()
        return runCatching {
            r.setAudioSource(MediaRecorder.AudioSource.VOICE_COMMUNICATION)
            if (opus) {
                r.setOutputFormat(MediaRecorder.OutputFormat.OGG)
                r.setAudioEncoder(MediaRecorder.AudioEncoder.OPUS)
                r.setAudioEncodingBitRate(16_000)
            } else {
                r.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
                r.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
                r.setAudioEncodingBitRate(24_000)
            }
            r.setAudioChannels(1)
            r.setAudioSamplingRate(if (opus) 48_000 else 44_100)
            r.setMaxFileSize(MAX_BYTES)
            r.setMaxDuration(MAX_MS)
            r.setOnInfoListener { _, what, _ ->
                if (what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_FILESIZE_REACHED ||
                    what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_DURATION_REACHED
                ) onLimit()
            }
            if (descriptor != null) r.setOutputFile(descriptor.fileDescriptor) else r.setOutputFile(requireNotNull(out).absolutePath)
            r.prepare()
            r.start()
        }.onFailure {
            r.release()
            out?.delete()
            descriptor?.close()
            capture?.close()
        }.onSuccess {
            recorder = r
            file = out
            privateCapture = capture
            outputDescriptor = descriptor
            startedAt = SystemClock.elapsedRealtime()
            samples.clear()
        }.isSuccess
    }

    val elapsedMs: Long get() = if (recorder == null) 0 else SystemClock.elapsedRealtime() - startedAt

    /** Loudness in 0f..1f. maxAmplitude resets on every read, so only the recording ticker calls this. */
    fun sample(): Float {
        val amp = runCatching { recorder?.maxAmplitude ?: 0 }.getOrDefault(0)
        samples += amp
        return (amp / 32767f).coerceIn(0f, 1f)
    }

    class Recording(val bytes: ByteArray, val mime: String, val durationMs: Int, val waveform: ByteArray)

    fun finish(): Recording? {
        val r = recorder ?: return null
        val f = file
        val duration = elapsedMs.toInt()
        val m = mime
        // A recorder that hit its own cap has already stopped and finalised
        // the file; stop() then throws, and the file is still the note.
        runCatching { r.stop() }
        val bytes = runCatching {
            privateCapture?.let { capture ->
                context.contentResolver.openInputStream(capture.uri)?.use { input ->
                    val bytes = input.readBytes()
                    require(bytes.size <= MAX_BYTES + 64 * 1024)
                    bytes
                }
            } ?: f?.readBytes()
        }.getOrNull()
        release()
        if (bytes == null || bytes.isEmpty() || duration < MIN_MS) return null
        return Recording(bytes, m, duration, waveformOf(samples))
    }

    fun cancel() {
        val r = recorder ?: return
        runCatching { r.stop() }
        release()
    }

    private fun release() {
        recorder?.release()
        recorder = null
        file?.delete()
        file = null
        runCatching { outputDescriptor?.close() }
        outputDescriptor = null
        privateCapture?.close()
        privateCapture = null
    }

    private fun waveformOf(raw: List<Int>): ByteArray {
        if (raw.isEmpty()) return ByteArray(0)
        val peak = raw.max().coerceAtLeast(1)
        return ByteArray(WAVE_BARS) { i ->
            val from = i * raw.size / WAVE_BARS
            val to = ((i + 1) * raw.size / WAVE_BARS).coerceAtLeast(from + 1).coerceAtMost(raw.size)
            val bucket = raw.subList(from, to)
            (bucket.max() * 255 / peak).coerceIn(0, 255).toByte()
        }
    }

    companion object {
        /** Under libcore's 256KB inline cap with room for container overhead. */
        const val MAX_BYTES = 240L * 1024
        const val MAX_MS = 2 * 60 * 1000
        /** A tap, not a note. */
        const val MIN_MS = 500
        const val WAVE_BARS = 48
    }
}

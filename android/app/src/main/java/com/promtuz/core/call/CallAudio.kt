package com.promtuz.core.call

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.os.Build
import android.os.Process
import com.promtuz.core.CoreBridge
import timber.log.Timber

/**
 * The microphone and speaker for a call. Capture reads 20 ms of 48 kHz mono
 * PCM per tick and hands it to core; playback pulls the same from core. Both
 * run in communication mode so the platform's own echo canceller and noise
 * suppressor are in the path — the same default Telegram relies on rather than
 * a canceller in the engine.
 */
class CallAudio(private val audioManager: AudioManager) {
    private companion object {
        const val SAMPLE_RATE = 48_000
        const val FRAME_SAMPLES = 960 // 20 ms
        const val FRAME_BYTES = FRAME_SAMPLES * 2
    }

    @Volatile private var running = false
    private var capture: Thread? = null
    private var playback: Thread? = null
    private var record: AudioRecord? = null
    private var track: AudioTrack? = null
    private var previousMode = AudioManager.MODE_NORMAL
    private var deviceCallback: AudioDeviceCallback? = null

    /** Route to the loudspeaker rather than the earpiece. */
    @Volatile var speaker = false
        set(value) {
            field = value
            applyRoute()
        }

    @SuppressLint("MissingPermission")
    fun start() {
        if (running) return
        running = true
        previousMode = audioManager.mode
        audioManager.mode = AudioManager.MODE_IN_COMMUNICATION
        applyRoute()
        // Re-route when a headset comes or goes mid-call.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val cb = object : AudioDeviceCallback() {
                override fun onAudioDevicesAdded(added: Array<out AudioDeviceInfo>) = applyRoute()
                override fun onAudioDevicesRemoved(removed: Array<out AudioDeviceInfo>) = applyRoute()
            }
            audioManager.registerAudioDeviceCallback(cb, null)
            deviceCallback = cb
        }

        val recordBytes = maxOf(
            AudioRecord.getMinBufferSize(
                SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            ),
            FRAME_BYTES * 4,
        )
        val rec = AudioRecord(
            MediaRecorder.AudioSource.VOICE_COMMUNICATION,
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, recordBytes,
        )
        val trackBytes = maxOf(
            AudioTrack.getMinBufferSize(
                SAMPLE_RATE, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT,
            ),
            FRAME_BYTES * 4,
        )
        val trk = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build(),
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build(),
            )
            .setBufferSizeInBytes(trackBytes)
            .build()
        record = rec
        track = trk

        if (rec.state != AudioRecord.STATE_INITIALIZED || trk.state != AudioTrack.STATE_INITIALIZED) {
            Timber.tag("Call").e("audio device did not initialise")
            stop()
            return
        }

        rec.startRecording()
        trk.play()
        capture = Thread({ captureLoop(rec) }, "call-capture").apply { start() }
        playback = Thread({ playbackLoop(trk) }, "call-playback").apply { start() }
    }

    fun stop() {
        if (!running) return
        running = false
        capture?.join(500)
        playback?.join(500)
        capture = null
        playback = null
        runCatching { record?.stop() }
        runCatching { track?.stop() }
        record?.release()
        track?.release()
        record = null
        track = null
        audioManager.mode = previousMode
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            deviceCallback?.let { audioManager.unregisterAudioDeviceCallback(it) }
            deviceCallback = null
            // Release our explicit route so the system resumes normal routing.
            runCatching { audioManager.clearCommunicationDevice() }
        } else {
            @Suppress("DEPRECATION")
            audioManager.isSpeakerphoneOn = false
        }
    }

    private fun captureLoop(rec: AudioRecord) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val buf = ByteArray(FRAME_BYTES)
        while (running) {
            var off = 0
            while (off < FRAME_BYTES && running) {
                val n = rec.read(buf, off, FRAME_BYTES - off)
                if (n <= 0) return
                off += n
            }
            if (off == FRAME_BYTES) CoreBridge.callPushAudio(buf)
        }
    }

    private fun playbackLoop(trk: AudioTrack) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        while (running) {
            val pcm = CoreBridge.callPullAudio(1)
            var off = 0
            while (off < pcm.size && running) {
                val n = trk.write(pcm, off, pcm.size - off)
                if (n < 0) return
                off += n
            }
        }
    }

    /** Point the audio at the loudspeaker, a headset, or the earpiece. Speaker
     *  forces the loudspeaker; otherwise a connected headset wins over the
     *  earpiece, so plugging in mid-call is respected. */
    private fun applyRoute() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val devices = audioManager.availableCommunicationDevices
            val device = if (speaker) {
                devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER }
            } else {
                devices.firstOrNull { it.type in HEADSET_TYPES }
                    ?: devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_EARPIECE }
            }
            if (device != null) audioManager.setCommunicationDevice(device)
        } else {
            @Suppress("DEPRECATION")
            audioManager.isSpeakerphoneOn = speaker
        }
    }
}

private val HEADSET_TYPES = intArrayOf(
    AudioDeviceInfo.TYPE_BLUETOOTH_SCO,
    AudioDeviceInfo.TYPE_WIRED_HEADSET,
    AudioDeviceInfo.TYPE_WIRED_HEADPHONES,
    AudioDeviceInfo.TYPE_USB_HEADSET,
)

package com.promtuz.core.call

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import timber.log.Timber
import java.util.ArrayDeque

/** Frames arrive on a core thread; the queues and the codec are touched only on the decoder thread. */
class CallVideoDecoder(private val surface: Surface) {
    private val thread = HandlerThread("call-decoder").apply { start() }
    private val handler = Handler(thread.looper)
    private val pending = ArrayDeque<ByteArray>()
    private val free = ArrayDeque<Int>()
    private var codec: MediaCodec? = null
    @Volatile private var started = false
    // The decoder starts on a keyframe, which carries the SPS/PPS, rather than mid-GOP.
    private var sawKeyframe = false

    fun start() {
        if (started) return
        started = true
        handler.post {
            // A nominal size; the real one comes from the stream's SPS.
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, 640, 480)
            val decoder = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            decoder.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                    val frame = pending.pollFirst()
                    if (frame == null) {
                        free.addLast(index)
                    } else {
                        feed(codec, index, frame)
                    }
                }
                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    runCatching { codec.releaseOutputBuffer(index, info.size != 0) }
                }
                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    Timber.tag("Call").w(e, "video decoder error")
                }
                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
            }, handler)
            decoder.configure(format, surface, null, 0)
            decoder.start()
            codec = decoder
        }
    }

    /** One encoded access unit from the peer, Annex-B. */
    fun submit(frame: ByteArray, keyframe: Boolean) {
        handler.post {
            if (!sawKeyframe) {
                if (!keyframe) return@post
                sawKeyframe = true
            }
            val index = free.pollFirst()
            val codec = codec
            if (index != null && codec != null) feed(codec, index, frame) else pending.addLast(frame)
        }
    }

    private fun feed(codec: MediaCodec, index: Int, frame: ByteArray) {
        val buffer = codec.getInputBuffer(index) ?: return
        buffer.clear()
        buffer.put(frame)
        runCatching {
            codec.queueInputBuffer(index, 0, frame.size, System.nanoTime() / 1000, 0)
        }
    }

    fun stop() {
        if (!started) return
        started = false
        handler.post {
            runCatching { codec?.stop() }
            runCatching { codec?.release() }
            codec = null
            thread.quitSafely()
        }
    }
}

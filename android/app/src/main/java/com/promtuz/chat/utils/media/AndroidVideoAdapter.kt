package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.graphics.SurfaceTexture
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaMetadataRetriever as PlatformMetadataRetriever
import android.media.MediaMuxer
import android.net.Uri
import android.opengl.EGL14
import android.opengl.EGLExt
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.view.Surface
import java.io.Closeable
import java.io.File
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.FloatBuffer
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit
import kotlin.math.roundToInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import uniffi.core.VideoAudioProbeRecord
import uniffi.core.VideoEncodingPlanRecord
import uniffi.core.VideoProbeRecord

class VideoPreparationException(message: String, cause: Throwable? = null) : IOException(message, cause)

private val videoExports = Mutex()

/** Android codec implementation used only by [AndroidMediaAdapter]. */
internal object AndroidVideoAdapter {
suspend fun inspect(context: Context, uri: Uri): VideoProbeRecord {
    try {
        val metadata = withContext(Dispatchers.IO) { platformVideoMetadata(context, uri) }
        val video = metadata.video
        val mime = video.getString(MediaFormat.KEY_MIME)
            ?: throw VideoPreparationException("The video format is unavailable.")
        val transfer = video.int(MediaFormat.KEY_COLOR_TRANSFER)?.takeIf { it > 0 }
        val primaries = video.int(MediaFormat.KEY_COLOR_STANDARD)?.takeIf { it > 0 }
        val range = video.int(MediaFormat.KEY_COLOR_RANGE)?.takeIf { it > 0 }
        val staticHdr = video.bytes(MediaFormat.KEY_HDR_STATIC_INFO)
        val profile = video.int(MediaFormat.KEY_PROFILE)
        val hdr10Plus = hasHdr10PlusProfile(mime, profile) || video.containsKey(MediaFormat.KEY_HDR10_PLUS_INFO)
        val hdr = transfer == MediaFormat.COLOR_TRANSFER_HLG || transfer == MediaFormat.COLOR_TRANSFER_ST2084 ||
            staticHdr != null || hdr10Plus || hasHdrProfile(mime, profile) || mime == "video/dolby-vision"
        val (width, height) = video.cropSize()
            ?: (video.int(MediaFormat.KEY_WIDTH) ?: 0) to (video.int(MediaFormat.KEY_HEIGHT) ?: 0)
        val rotation = ((video.int(MediaFormat.KEY_ROTATION) ?: 0) % 360 + 360) % 360
        if (rotation % 90 != 0) throw VideoPreparationException("The video's orientation could not be read.")
        return VideoProbeRecord(
            containerMime = metadata.containerMime,
            videoMime = mime,
            videoTracks = metadata.videoTracks.toUInt(),
            audioTracks = metadata.audioTracks.toUInt(),
            displayWidth = if (rotation % 180 == 0) width.toDouble() else height.toDouble(),
            displayHeight = if (rotation % 180 == 0) height.toDouble() else width.toDouble(),
            durationUs = metadata.durationUs.toULong(),
            frameRate = video.number(MediaFormat.KEY_FRAME_RATE)?.takeIf { it.isFinite() && it > 0 },
            videoBitrate = video.int(MediaFormat.KEY_BIT_RATE)?.takeIf { it > 0 }?.toUInt(),
            audio = metadata.audio?.let {
                VideoAudioProbeRecord(
                    mime = it.getString(MediaFormat.KEY_MIME).orEmpty(),
                    durationUs = null,
                    bitrate = it.int(MediaFormat.KEY_BIT_RATE)?.takeIf { bitrate -> bitrate > 0 }?.toUInt(),
                    channels = it.int(MediaFormat.KEY_CHANNEL_COUNT)?.takeIf { channels -> channels > 0 }?.toUInt(),
                )
            },
            hdr = hdr,
            dynamicHdr = hdr10Plus || mime == "video/dolby-vision",
            colorPrimaries = primaries?.let(::isoPrimaries),
            colorTransfer = transfer?.let(::isoTransfer),
            fullRange = when (range) { MediaFormat.COLOR_RANGE_FULL -> true; MediaFormat.COLOR_RANGE_LIMITED -> false; else -> null },
            bitDepth = video.int("bit-depth")?.takeIf { it in 1..255 }?.toUByte(),
            hdrStaticMetadata = staticHdr,
        )
    } catch (error: CancellationException) {
        throw error
    } catch (error: VideoPreparationException) {
        throw error
    } catch (error: Exception) {
        throw VideoPreparationException("The video's media information could not be read.", error)
    }
}

/** Decodes one displayed frame for core's bounded sticker timeline. */
internal class VideoFrameDecoder private constructor(
    private val retriever: PlatformMetadataRetriever,
) : Closeable {
    suspend fun frame(timestampUs: ULong, maxEdge: Int): Bitmap = withContext(Dispatchers.IO) {
        if (timestampUs > Long.MAX_VALUE.toULong() || maxEdge <= 0) {
            throw VideoPreparationException("The video frame request is invalid.")
        }
        try {
            val frame = retriever.getFrameAtTime(
                timestampUs.toLong(),
                PlatformMetadataRetriever.OPTION_CLOSEST,
            ) ?: throw VideoPreparationException("A frame from this video could not be decoded.")
            val (width, height) = fit(frame.width, frame.height, maxEdge)
            if (width == frame.width && height == frame.height) frame
            else Bitmap.createScaledBitmap(frame, width, height, true).also { frame.recycle() }
        } catch (error: VideoPreparationException) {
            throw error
        } catch (error: Exception) {
            throw VideoPreparationException("A frame from this video could not be decoded.", error)
        }
    }

    override fun close() = retriever.release()

    companion object {
        suspend fun open(context: Context, uri: Uri): VideoFrameDecoder = withContext(Dispatchers.IO) {
            val retriever = PlatformMetadataRetriever()
            try {
                retriever.setDataSource(context, uri)
                VideoFrameDecoder(retriever)
            } catch (error: Exception) {
                retriever.release()
                throw VideoPreparationException("The video frames could not be opened.", error)
            }
        }
    }
}

suspend fun render(context: Context, source: Uri, output: File, input: VideoProbeRecord, plan: VideoEncodingPlanRecord) {
    videoExports.withLock {
        withContext(Dispatchers.Default) {
            PlatformVideoTranscoder(context, source, output, input, plan).run()
        }
    }
}
}

private class PlatformVideoTranscoder(
    private val context: Context,
    private val source: Uri,
    private val output: File,
    private val input: VideoProbeRecord,
    private val plan: VideoEncodingPlanRecord,
) {
    fun run() {
        val extractor = MediaExtractor()
        var decoder: MediaCodec? = null
        var encoder: MediaCodec? = null
        var bridge: CodecSurfaceBridge? = null
        var muxer: MediaMuxer? = null
        try {
            extractor.setDataSource(context, source, emptyMap())
            val track = extractor.videoTrack()
            val sourceFormat = extractor.getTrackFormat(track)
            val sourceMime = sourceFormat.getString(MediaFormat.KEY_MIME)
                ?: throw VideoPreparationException("The video's picture format is unavailable.")
            val audio = encodeAudio()
            val width = plan.width.codecInt()
            val height = plan.height.codecInt()
            val encoderFormat = MediaFormat.createVideoFormat(plan.videoMime, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, plan.videoBitrate.codecInt())
                setInteger(MediaFormat.KEY_FRAME_RATE, plan.maxFrameRate.codecInt())
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
                setInteger(MediaFormat.KEY_LEVEL, MediaCodecInfo.CodecProfileLevel.AVCLevel31)
            }
            encoder = MediaCodec.createEncoderByType(plan.videoMime)
            encoder.configure(encoderFormat, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            bridge = CodecSurfaceBridge(encoder.createInputSurface(), width, height, sourceFormat.rotation())
            decoder = MediaCodec.createDecoderByType(sourceMime)
            if (input.hdr) sourceFormat.setInteger(MediaFormat.KEY_COLOR_TRANSFER_REQUEST, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
            decoder.configure(sourceFormat, bridge.decoderSurface, null, 0)
            muxer = MediaMuxer(output.absolutePath, MediaMuxer.OutputFormat.MUXER_OUTPUT_MPEG_4)
            extractor.selectTrack(track)
            decoder.start()
            encoder.start()
            transcode(extractor, decoder, encoder, bridge, muxer, plan.maxFrameRate.codecInt(), audio)
            muxer.stop()
        } catch (error: VideoPreparationException) {
            throw error
        } catch (error: Exception) {
            throw VideoPreparationException("The video could not be encoded on this device.", error)
        } finally {
            runCatching { decoder?.stop() }
            runCatching { encoder?.stop() }
            runCatching { decoder?.release() }
            runCatching { encoder?.release() }
            runCatching { bridge?.close() }
            runCatching { muxer?.release() }
            extractor.release()
        }
    }

    private fun transcode(
        extractor: MediaExtractor,
        decoder: MediaCodec,
        encoder: MediaCodec,
        bridge: CodecSurfaceBridge,
        muxer: MediaMuxer,
        maxFrameRate: Int,
        audio: EncodedAudio?,
    ) {
        val decoderInfo = MediaCodec.BufferInfo()
        val encoderInfo = MediaCodec.BufferInfo()
        var inputDone = false
        var decoderDone = false
        var encoderDone = false
        var muxerStarted = false
        var videoTrack = -1
        var lastRenderedUs = Long.MIN_VALUE
        val minFrameDurationUs = 1_000_000L / maxFrameRate

        while (!encoderDone) {
            if (!inputDone) {
                val index = decoder.dequeueInputBuffer(CODEC_TIMEOUT_US)
                if (index >= 0) {
                    val buffer = decoder.getInputBuffer(index)
                        ?: throw VideoPreparationException("The video decoder did not provide input memory.")
                    val size = extractor.readSampleData(buffer, 0)
                    if (size < 0) {
                        decoder.queueInputBuffer(index, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                        inputDone = true
                    } else {
                        decoder.queueInputBuffer(index, 0, size, extractor.sampleTime, 0)
                        extractor.advance()
                    }
                }
            }

            while (!decoderDone) {
                when (val index = decoder.dequeueOutputBuffer(decoderInfo, CODEC_TIMEOUT_US)) {
                    MediaCodec.INFO_TRY_AGAIN_LATER -> break
                    MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> Unit
                    else -> if (index >= 0) {
                        val eos = decoderInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0
                        val render = decoderInfo.size > 0 &&
                            (lastRenderedUs == Long.MIN_VALUE || decoderInfo.presentationTimeUs - lastRenderedUs >= minFrameDurationUs)
                        decoder.releaseOutputBuffer(index, render)
                        if (render) {
                            bridge.awaitFrame()
                            bridge.drawFrame(decoderInfo.presentationTimeUs)
                            lastRenderedUs = decoderInfo.presentationTimeUs
                        }
                        if (eos) {
                            encoder.signalEndOfInputStream()
                            decoderDone = true
                        }
                    }
                }
            }

            while (!encoderDone) {
                when (val index = encoder.dequeueOutputBuffer(encoderInfo, CODEC_TIMEOUT_US)) {
                    MediaCodec.INFO_TRY_AGAIN_LATER -> break
                    MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                        if (muxerStarted) throw VideoPreparationException("The video encoder changed format twice.")
                        videoTrack = muxer.addTrack(encoder.outputFormat)
                        val audioTrack = audio?.let { muxer.addTrack(it.format) }
                        muxer.start()
                        muxerStarted = true
                        if (audioTrack != null) audio.samples.forEach { sample ->
                            muxer.writeSampleData(audioTrack, ByteBuffer.wrap(sample.data), sample.info())
                        }
                    }
                    else -> if (index >= 0) {
                        val buffer = encoder.getOutputBuffer(index)
                            ?: throw VideoPreparationException("The video encoder did not provide output memory.")
                        if (encoderInfo.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 && encoderInfo.size > 0) {
                            if (!muxerStarted) throw VideoPreparationException("The video encoder produced samples before its format.")
                            buffer.position(encoderInfo.offset)
                            buffer.limit(encoderInfo.offset + encoderInfo.size)
                            muxer.writeSampleData(videoTrack, buffer, encoderInfo)
                        }
                        encoder.releaseOutputBuffer(index, false)
                        if (encoderInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) encoderDone = true
                    }
                }
            }
        }
        if (!muxerStarted) throw VideoPreparationException("The video encoder produced no output.")
    }

    private fun encodeAudio(): EncodedAudio? = try {
        val extractor = MediaExtractor()
        var decoder: MediaCodec? = null
        var encoder: MediaCodec? = null
        try {
            extractor.setDataSource(context, source, emptyMap())
            val track = (0 until extractor.trackCount).firstOrNull { index ->
                extractor.getTrackFormat(index).getString(MediaFormat.KEY_MIME)?.startsWith("audio/") == true
            } ?: return null
            val input = extractor.getTrackFormat(track)
            val inputMime = input.getString(MediaFormat.KEY_MIME)
                ?: return null
            extractor.selectTrack(track)
            decoder = MediaCodec.createDecoderByType(inputMime).apply { configure(input, null, null, 0); start() }
            val decoderInfo = MediaCodec.BufferInfo()
            val encoderInfo = MediaCodec.BufferInfo()
            val samples = mutableListOf<EncodedSample>()
            var sampleBytes = 0L
            var format: MediaFormat? = null
            var decoderInputDone = false
            var decoderDone = false
            var encoderDone = false
            while (!encoderDone) {
                if (!decoderInputDone) {
                    val index = decoder.dequeueInputBuffer(CODEC_TIMEOUT_US)
                    if (index >= 0) {
                        val buffer = decoder.getInputBuffer(index) ?: return null
                        val size = extractor.readSampleData(buffer, 0)
                        if (size < 0) {
                            decoder.queueInputBuffer(index, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                            decoderInputDone = true
                        } else {
                            decoder.queueInputBuffer(index, 0, size, extractor.sampleTime, 0)
                            extractor.advance()
                        }
                    }
                }
                while (!decoderDone) when (val index = decoder.dequeueOutputBuffer(decoderInfo, CODEC_TIMEOUT_US)) {
                    MediaCodec.INFO_TRY_AGAIN_LATER -> break
                    MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                        val pcm = decoder.outputFormat
                        val rate = pcm.int(MediaFormat.KEY_SAMPLE_RATE) ?: return null
                        val channels = pcm.int(MediaFormat.KEY_CHANNEL_COUNT) ?: return null
                        if (rate !in 8_000..48_000 || channels !in 1..2) return null
                        encoder = MediaCodec.createEncoderByType(plan.audioMime).apply {
                            configure(MediaFormat.createAudioFormat(plan.audioMime, rate, channels).apply {
                                setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
                                setInteger(MediaFormat.KEY_BIT_RATE, plan.audioBitrate.codecInt())
                                setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 64 * 1024)
                            }, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                            start()
                        }
                    }
                    else -> if (index >= 0) {
                        val eos = decoderInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0
                        if (decoderInfo.size > 0) {
                            val audioEncoder = encoder ?: return null
                            val inputIndex = audioEncoder.dequeueInputBuffer(CODEC_TIMEOUT_US)
                            if (inputIndex < 0) continue
                            val encoded = audioEncoder.getInputBuffer(inputIndex) ?: return null
                            val decoded = decoder.getOutputBuffer(index) ?: return null
                            decoded.position(decoderInfo.offset)
                            decoded.limit(decoderInfo.offset + decoderInfo.size)
                            encoded.put(decoded)
                            audioEncoder.queueInputBuffer(inputIndex, 0, decoderInfo.size, decoderInfo.presentationTimeUs, 0)
                        }
                        decoder.releaseOutputBuffer(index, false)
                        if (eos) {
                            encoder?.let { audioEncoder ->
                                val inputIndex = audioEncoder.dequeueInputBuffer(CODEC_TIMEOUT_US)
                                if (inputIndex >= 0) audioEncoder.queueInputBuffer(inputIndex, 0, 0, decoderInfo.presentationTimeUs, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                            }
                            decoderDone = true
                        }
                    }
                }
                val audioEncoder = encoder
                if (audioEncoder != null) while (!encoderDone) when (val index = audioEncoder.dequeueOutputBuffer(encoderInfo, CODEC_TIMEOUT_US)) {
                    MediaCodec.INFO_TRY_AGAIN_LATER -> break
                    MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> format = audioEncoder.outputFormat
                    else -> if (index >= 0) {
                        if (encoderInfo.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 && encoderInfo.size > 0) {
                            val buffer = audioEncoder.getOutputBuffer(index) ?: return null
                            buffer.position(encoderInfo.offset)
                            buffer.limit(encoderInfo.offset + encoderInfo.size)
                            val size = buffer.remaining()
                            if (sampleBytes + size > MAX_CACHED_AUDIO_BYTES) return null
                            samples += EncodedSample(ByteArray(size).also(buffer::get), encoderInfo.presentationTimeUs, encoderInfo.flags)
                            sampleBytes += size
                        }
                        audioEncoder.releaseOutputBuffer(index, false)
                        if (encoderInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) encoderDone = true
                    }
                }
            }
            format?.let { EncodedAudio(it, samples) }
        } finally {
            runCatching { decoder?.stop() }; runCatching { encoder?.stop() }
            runCatching { decoder?.release() }; runCatching { encoder?.release() }; extractor.release()
        }
    } catch (_: Exception) {
        null
    }

    private data class EncodedAudio(val format: MediaFormat, val samples: List<EncodedSample>)
    private data class EncodedSample(val data: ByteArray, val presentationTimeUs: Long, val flags: Int) {
        fun info() = MediaCodec.BufferInfo().also { it.set(0, data.size, presentationTimeUs, flags) }
    }

    private fun MediaExtractor.videoTrack(): Int = (0 until trackCount).firstOrNull { index ->
        getTrackFormat(index).getString(MediaFormat.KEY_MIME)?.startsWith("video/") == true
    } ?: throw VideoPreparationException("The selected file has no readable video track.")

    private fun MediaFormat.rotation(): Int = (int(MediaFormat.KEY_ROTATION) ?: 0).let { (it % 360 + 360) % 360 }

    private companion object {
        const val CODEC_TIMEOUT_US = 10_000L
        const val MAX_CACHED_AUDIO_BYTES = 32L * 1024 * 1024
    }
}

private class CodecSurfaceBridge(
    encoderSurface: Surface,
    width: Int,
    height: Int,
    rotation: Int,
) : Closeable {
    val decoderSurface: Surface
    private val texture: Int
    private val surfaceTexture: SurfaceTexture
    private val display = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
    private val context: android.opengl.EGLContext
    private val window: android.opengl.EGLSurface
    private val program: Int
    private val position: Int
    private val textureCoordinate: Int
    private val transform: Int
    private val textureMatrix = FloatArray(16)
    private val vertices = vertexData()
    private val textureCoordinates = textureData(rotation)
    private val frames = Semaphore(0)

    init {
        check(display != EGL14.EGL_NO_DISPLAY) { "No EGL display" }
        val version = IntArray(2)
        check(EGL14.eglInitialize(display, version, 0, version, 1)) { "Could not initialize EGL" }
        val configs = arrayOfNulls<android.opengl.EGLConfig>(1)
        val count = IntArray(1)
        check(EGL14.eglChooseConfig(display, intArrayOf(
            EGL14.EGL_RED_SIZE, 8, EGL14.EGL_GREEN_SIZE, 8, EGL14.EGL_BLUE_SIZE, 8,
            EGL14.EGL_ALPHA_SIZE, 8, EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
            EGL_RECORDABLE_ANDROID, 1, EGL14.EGL_NONE,
        ), 0, configs, 0, 1, count, 0) && count[0] == 1) { "No recordable EGL config" }
        context = EGL14.eglCreateContext(display, configs[0], EGL14.EGL_NO_CONTEXT,
            intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE), 0)
        check(context != EGL14.EGL_NO_CONTEXT) { "Could not create EGL context" }
        window = EGL14.eglCreateWindowSurface(display, configs[0], encoderSurface,
            intArrayOf(EGL14.EGL_NONE), 0)
        check(window != EGL14.EGL_NO_SURFACE) { "Could not create EGL encoder surface" }
        makeCurrent()
        GLES20.glViewport(0, 0, width, height)
        texture = IntArray(1).also { GLES20.glGenTextures(1, it, 0) }[0]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, texture)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
        surfaceTexture = SurfaceTexture(texture).also { image ->
            image.setOnFrameAvailableListener { frames.release() }
        }
        decoderSurface = Surface(surfaceTexture)
        program = program(VERTEX_SHADER, FRAGMENT_SHADER)
        position = GLES20.glGetAttribLocation(program, "aPosition")
        textureCoordinate = GLES20.glGetAttribLocation(program, "aTextureCoordinate")
        transform = GLES20.glGetUniformLocation(program, "uTextureTransform")
    }

    fun awaitFrame() {
        if (!frames.tryAcquire(FRAME_WAIT_MS, TimeUnit.MILLISECONDS)) {
            throw VideoPreparationException("The video decoder stopped producing frames.")
        }
    }

    fun drawFrame(presentationTimeUs: Long) {
        makeCurrent()
        surfaceTexture.updateTexImage()
        surfaceTexture.getTransformMatrix(textureMatrix)
        GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
        GLES20.glUseProgram(program)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, texture)
        GLES20.glUniformMatrix4fv(transform, 1, false, textureMatrix, 0)
        GLES20.glEnableVertexAttribArray(position)
        GLES20.glVertexAttribPointer(position, 2, GLES20.GL_FLOAT, false, 4 * Float.SIZE_BYTES, vertices)
        GLES20.glEnableVertexAttribArray(textureCoordinate)
        GLES20.glVertexAttribPointer(textureCoordinate, 2, GLES20.GL_FLOAT, false, 4 * Float.SIZE_BYTES, textureCoordinates)
        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
        EGLExt.eglPresentationTimeANDROID(display, window, presentationTimeUs * 1_000)
        check(EGL14.eglSwapBuffers(display, window)) { "Could not submit a video frame" }
    }

    private fun makeCurrent() {
        check(EGL14.eglMakeCurrent(display, window, window, context)) { "Could not make EGL context current" }
    }

    override fun close() {
        runCatching { decoderSurface.release() }
        runCatching { surfaceTexture.release() }
        runCatching { GLES20.glDeleteProgram(program) }
        runCatching { GLES20.glDeleteTextures(1, intArrayOf(texture), 0) }
        runCatching { EGL14.eglDestroySurface(display, window) }
        runCatching { EGL14.eglDestroyContext(display, context) }
        runCatching { EGL14.eglTerminate(display) }
    }

    private companion object {
        const val EGL_RECORDABLE_ANDROID = 0x3142
        const val FRAME_WAIT_MS = 2_500L
        const val VERTEX_SHADER = """
            attribute vec4 aPosition;
            attribute vec4 aTextureCoordinate;
            varying vec2 vTextureCoordinate;
            uniform mat4 uTextureTransform;
            void main() { gl_Position = aPosition; vTextureCoordinate = (uTextureTransform * aTextureCoordinate).xy; }
        """
        const val FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            varying vec2 vTextureCoordinate;
            uniform samplerExternalOES sTexture;
            void main() { gl_FragColor = texture2D(sTexture, vTextureCoordinate); }
        """

        fun program(vertex: String, fragment: String): Int {
            fun shader(type: Int, source: String) = GLES20.glCreateShader(type).also { shader ->
                GLES20.glShaderSource(shader, source)
                GLES20.glCompileShader(shader)
                check(IntArray(1).also { GLES20.glGetShaderiv(shader, GLES20.GL_COMPILE_STATUS, it, 0) }[0] == GLES20.GL_TRUE) {
                    GLES20.glGetShaderInfoLog(shader)
                }
            }
            return GLES20.glCreateProgram().also { program ->
                GLES20.glAttachShader(program, shader(GLES20.GL_VERTEX_SHADER, vertex))
                GLES20.glAttachShader(program, shader(GLES20.GL_FRAGMENT_SHADER, fragment))
                GLES20.glLinkProgram(program)
                check(IntArray(1).also { GLES20.glGetProgramiv(program, GLES20.GL_LINK_STATUS, it, 0) }[0] == GLES20.GL_TRUE) {
                    GLES20.glGetProgramInfoLog(program)
                }
            }
        }

        fun vertexData(): FloatBuffer = floatBuffer(floatArrayOf(
            -1f, -1f, 1f, -1f, -1f, 1f, 1f, 1f,
        ))

        fun textureData(rotation: Int): FloatBuffer = floatBuffer(when (rotation) {
            90 -> floatArrayOf(0f, 0f, 0f, 1f, 1f, 0f, 1f, 1f)
            180 -> floatArrayOf(1f, 0f, 0f, 0f, 1f, 1f, 0f, 1f)
            270 -> floatArrayOf(1f, 1f, 1f, 0f, 0f, 1f, 0f, 0f)
            else -> floatArrayOf(0f, 1f, 1f, 1f, 0f, 0f, 1f, 0f)
        })

        fun floatBuffer(values: FloatArray): FloatBuffer = ByteBuffer.allocateDirect(values.size * Float.SIZE_BYTES)
            .order(java.nio.ByteOrder.nativeOrder()).asFloatBuffer().apply { put(values); position(0) }
    }
}

private data class PlatformVideoMetadata(
    val containerMime: String?,
    val video: MediaFormat,
    val audio: MediaFormat?,
    val videoTracks: Int,
    val audioTracks: Int,
    val durationUs: Long,
)

private fun platformVideoMetadata(context: Context, uri: Uri): PlatformVideoMetadata {
    val extractor = MediaExtractor()
    var video: MediaFormat? = null
    var audio: MediaFormat? = null
    var videoTracks = 0
    var audioTracks = 0
    try {
        extractor.setDataSource(context, uri, emptyMap())
        for (index in 0 until extractor.trackCount) {
            val format = extractor.getTrackFormat(index)
            val mime = format.getString(MediaFormat.KEY_MIME).orEmpty()
            if (mime.startsWith("video/")) {
                videoTracks += 1
                if (video == null) video = format
            }
            if (mime.startsWith("audio/")) {
                audioTracks += 1
                if (audio == null) audio = format
            }
        }
    } finally {
        extractor.release()
    }
    val containerMime = runCatching {
        val retriever = PlatformMetadataRetriever()
        try {
            retriever.setDataSource(context, uri)
            retriever.extractMetadata(PlatformMetadataRetriever.METADATA_KEY_MIMETYPE)
        } finally { retriever.release() }
    }.getOrNull()
    val source = video ?: throw VideoPreparationException("The video's picture track could not be read.")
    val durationUs = runCatching {
        val retriever = PlatformMetadataRetriever()
        try {
            retriever.setDataSource(context, uri)
            retriever.extractMetadata(PlatformMetadataRetriever.METADATA_KEY_DURATION)
                ?.toLongOrNull()?.let { milliseconds ->
                    milliseconds.coerceAtMost(Long.MAX_VALUE / 1_000) * 1_000
                }
        } finally {
            retriever.release()
        }
    }.getOrNull() ?: source.long(MediaFormat.KEY_DURATION) ?: 0
    return PlatformVideoMetadata(containerMime, source, audio, videoTracks, audioTracks, durationUs)
}

private fun MediaFormat.cropSize(): Pair<Int, Int>? {
    val width = int(MediaFormat.KEY_WIDTH) ?: return null
    val height = int(MediaFormat.KEY_HEIGHT) ?: return null
    val left = int(MediaFormat.KEY_CROP_LEFT) ?: return null
    val right = int(MediaFormat.KEY_CROP_RIGHT) ?: return null
    val top = int(MediaFormat.KEY_CROP_TOP) ?: return null
    val bottom = int(MediaFormat.KEY_CROP_BOTTOM) ?: return null
    return if (left in 0..right && right < width && top in 0..bottom && bottom < height) {
        (right - left + 1) to (bottom - top + 1)
    } else null
}

private fun isoPrimaries(value: Int): UShort? = when (value) {
    MediaFormat.COLOR_STANDARD_BT709 -> 1u
    MediaFormat.COLOR_STANDARD_BT601_PAL -> 5u
    MediaFormat.COLOR_STANDARD_BT601_NTSC -> 6u
    MediaFormat.COLOR_STANDARD_BT2020 -> 9u
    else -> null
}

private const val COLOR_TRANSFER_GAMMA_2_2 = 4
private const val COLOR_TRANSFER_SRGB = 7

private fun isoTransfer(value: Int): UShort? = when (value) {
    MediaFormat.COLOR_TRANSFER_LINEAR -> 8u
    COLOR_TRANSFER_SRGB -> 13u
    MediaFormat.COLOR_TRANSFER_SDR_VIDEO -> 1u
    COLOR_TRANSFER_GAMMA_2_2 -> 4u
    MediaFormat.COLOR_TRANSFER_ST2084 -> 16u
    MediaFormat.COLOR_TRANSFER_HLG -> 18u
    else -> null
}

private fun hasHdr10PlusProfile(mime: String, profile: Int?): Boolean = when (mime) {
    "video/hevc" -> profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10HDR10Plus
    "video/x-vnd.on2.vp9" -> profile in setOf(MediaCodecInfo.CodecProfileLevel.VP9Profile2HDR10Plus, MediaCodecInfo.CodecProfileLevel.VP9Profile3HDR10Plus)
    "video/av01" -> profile == MediaCodecInfo.CodecProfileLevel.AV1ProfileMain10HDR10Plus
    else -> false
}

private fun hasHdrProfile(mime: String, profile: Int?): Boolean = when (mime) {
    "video/hevc" -> profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10HDR10
    "video/x-vnd.on2.vp9" -> profile in setOf(MediaCodecInfo.CodecProfileLevel.VP9Profile2HDR, MediaCodecInfo.CodecProfileLevel.VP9Profile3HDR)
    "video/av01" -> profile == MediaCodecInfo.CodecProfileLevel.AV1ProfileMain10HDR10
    else -> false
}

private fun UInt.codecInt(): Int {
    if (this == 0u || this > Int.MAX_VALUE.toUInt()) throw VideoPreparationException("The video encoder cannot represent the requested settings.")
    return toInt()
}

private fun MediaFormat.int(key: String): Int? = runCatching { if (containsKey(key)) getInteger(key) else null }.getOrNull()
private fun MediaFormat.long(key: String): Long? = runCatching { if (containsKey(key)) getLong(key) else null }.getOrNull()
private fun MediaFormat.number(key: String): Double? = runCatching { if (containsKey(key)) getFloat(key).toDouble() else null }
    .getOrElse { int(key)?.toDouble() }
private fun MediaFormat.bytes(key: String): ByteArray? = runCatching {
    val buffer = if (containsKey(key)) getByteBuffer(key)?.duplicate() else null
    if (buffer == null || buffer.remaining() !in 1..1_048_576) null else ByteArray(buffer.remaining()).also { buffer.get(it) }
}.getOrNull()

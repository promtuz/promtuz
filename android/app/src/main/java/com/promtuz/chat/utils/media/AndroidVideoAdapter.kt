package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.media.MediaCodecInfo
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaMetadataRetriever as PlatformMetadataRetriever
import android.media.metrics.LogSessionId
import android.net.Uri
import android.os.Looper
import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MediaItem
import androidx.media3.common.util.UnstableApi
import androidx.media3.effect.Presentation
import androidx.media3.transformer.AudioEncoderSettings
import androidx.media3.transformer.Codec
import androidx.media3.transformer.Composition
import androidx.media3.transformer.DefaultEncoderFactory
import androidx.media3.transformer.EditedMediaItem
import androidx.media3.transformer.EditedMediaItemSequence
import androidx.media3.transformer.Effects
import androidx.media3.transformer.EncoderSelector
import androidx.media3.transformer.EncoderUtil
import androidx.media3.transformer.ExportException
import androidx.media3.transformer.ExportResult
import androidx.media3.transformer.TransformationRequest
import androidx.media3.transformer.Transformer
import androidx.media3.transformer.VideoEncoderSettings
import com.google.common.collect.ImmutableList
import com.google.common.collect.ImmutableSet
import java.io.Closeable
import java.io.File
import java.io.IOException
import kotlin.math.roundToInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
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
@androidx.annotation.OptIn(UnstableApi::class)
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
        val hdr = transfer == C.COLOR_TRANSFER_HLG || transfer == C.COLOR_TRANSFER_ST2084 ||
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
            fullRange = when (range) { C.COLOR_RANGE_FULL -> true; C.COLOR_RANGE_LIMITED -> false; else -> null },
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

/**
 * Executes core's plan without choosing settings, validating a result, or deleting files. All codec
 * calls and callbacks share the main looper. Cancellation releases the codec before returning so
 * core can safely clean up its source/candidate files.
 */
@androidx.annotation.OptIn(UnstableApi::class)
suspend fun render(context: Context, source: Uri, output: File, input: VideoProbeRecord, plan: VideoEncodingPlanRecord) {
    videoExports.withLock {
        val selector = EncoderSelector { mime ->
            val candidates = if (mime == plan.videoMime) EncoderUtil.getSupportedEncoders(mime) else emptyList()
            ImmutableList.copyOf(candidates.sortedByDescending { EncoderUtil.isHardwareAccelerated(it, mime) })
        }
        val supported = withContext(Dispatchers.Default) { selector.selectEncoderInfos(plan.videoMime).isNotEmpty() }
        if (!supported) throw VideoPreparationException("This device cannot encode the requested video format.")

        withContext(Dispatchers.Main.immediate) {
            val completion = CompletableDeferred<Unit>()
            var transformer: Transformer? = null
            var failure: Throwable? = null
            try {
                val maxFrameRate = plan.maxFrameRate.codecInt()
                val width = plan.width.codecInt()
                val height = plan.height.codecInt()
                val defaultFactory = DefaultEncoderFactory.Builder(context.applicationContext)
                    .setVideoEncoderSelector(selector)
                    .setEnableFallback(true)
                    // Alignment is a hardware constraint; core checks the resulting dimensions.
                    .setEnableFormatFallback(true)
                    // Non-default encoder settings force re-encoding of matching H.264/AAC inputs.
                    .setRequestedVideoEncoderSettings(VideoEncoderSettings.Builder().setBitrate(plan.videoBitrate.codecInt()).build())
                    .setRequestedAudioEncoderSettings(AudioEncoderSettings.Builder().setBitrate(plan.audioBitrate.codecInt()).build())
                    .build()
                val factory = object : Codec.EncoderFactory by defaultFactory {
                    override fun createForVideoEncoding(format: Format, logSessionId: LogSessionId?): Codec {
                        // Media3 drops frames separately but otherwise forwards the input FPS to
                        // the codec. Keep its declared frame rate consistent with core's ceiling.
                        val frameRate = format.frameRate.takeIf { it > 0 && it.isFinite() }
                            ?.coerceAtMost(maxFrameRate.toFloat()) ?: maxFrameRate.toFloat()
                        return defaultFactory.createForVideoEncoding(format.buildUpon().setFrameRate(frameRate).build(), logSessionId)
                    }
                }
                transformer = Transformer.Builder(context.applicationContext)
                    .setLooper(Looper.getMainLooper())
                    .setUsePlatformDiagnostics(false)
                    .setEncoderFactory(factory)
                    .setVideoMimeType(plan.videoMime)
                    .setAudioMimeType(plan.audioMime)
                    .addListener(object : Transformer.Listener {
                        override fun onCompleted(composition: Composition, exportResult: ExportResult) {
                            // Encoder names/frame statistics are telemetry, not file validity.
                            completion.complete(Unit)
                        }

                        override fun onError(composition: Composition, exportResult: ExportResult, exportException: ExportException) {
                            completion.completeExceptionally(VideoPreparationException(
                                if (plan.keepHdr) "This device could not encode the video while preserving HDR."
                                else "The video could not be encoded.", exportException,
                            ))
                        }

                        override fun onFallbackApplied(composition: Composition,
                            originalTransformationRequest: TransformationRequest,
                            fallbackTransformationRequest: TransformationRequest) {
                            if (fallbackTransformationRequest.hdrMode !in listOf(
                                    Composition.HDR_MODE_TONE_MAP_HDR_TO_SDR_USING_OPEN_GL,
                                    Composition.HDR_MODE_TONE_MAP_HDR_TO_SDR_USING_MEDIACODEC,
                                ) ||
                                fallbackTransformationRequest.videoMimeType != plan.videoMime ||
                                fallbackTransformationRequest.audioMimeType != plan.audioMime) {
                                completion.completeExceptionally(VideoPreparationException(
                                    "This device cannot produce the requested video format.",
                                ))
                            }
                        }
                    })
                    .build()
                val effects = if (width != input.displayWidth.roundToInt() || height != input.displayHeight.roundToInt()) {
                    listOf(Presentation.createForWidthAndHeight(width, height, Presentation.LAYOUT_SCALE_TO_FIT))
                } else emptyList()
                val item = EditedMediaItem.Builder(MediaItem.fromUri(source))
                    .setFrameRate(maxFrameRate)
                    .setEffects(Effects(emptyList(), effects))
                    .build()
                val composition = Composition.Builder(
                    EditedMediaItemSequence.Builder(
                        ImmutableSet.of(C.TRACK_TYPE_AUDIO, C.TRACK_TYPE_VIDEO),
                    ).addItem(item).build(),
                ).setHdrMode(Composition.HDR_MODE_TONE_MAP_HDR_TO_SDR_USING_OPEN_GL).build()
                transformer.start(composition, output.absolutePath)
                completion.await()
            } catch (error: Throwable) {
                failure = error
                throw error
            } finally {
                completion.cancel()
                try {
                    withContext(NonCancellable + Dispatchers.Main.immediate) {
                        // Media3 1.11.1 blocks here until its internal codecs and muxer release.
                        transformer?.cancel()
                    }
                } catch (error: Exception) {
                    failure?.addSuppressed(error) ?: throw error
                }
            }
        }
    }
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

@androidx.annotation.OptIn(UnstableApi::class)
private fun isoPrimaries(value: Int): UShort? = when (value) {
    C.COLOR_SPACE_BT709 -> 1u
    C.COLOR_SPACE_BT601 -> 5u
    MediaFormat.COLOR_STANDARD_BT601_NTSC -> 6u
    C.COLOR_SPACE_BT2020 -> 9u
    else -> null
}

@androidx.annotation.OptIn(UnstableApi::class)
private fun isoTransfer(value: Int): UShort? = when (value) {
    C.COLOR_TRANSFER_LINEAR -> 8u
    C.COLOR_TRANSFER_SRGB -> 13u
    C.COLOR_TRANSFER_SDR -> 1u
    C.COLOR_TRANSFER_GAMMA_2_2 -> 4u
    C.COLOR_TRANSFER_ST2084 -> 16u
    C.COLOR_TRANSFER_HLG -> 18u
    else -> null
}

@androidx.annotation.OptIn(UnstableApi::class)
private fun hasHdr10PlusProfile(mime: String, profile: Int?): Boolean = when (mime) {
    "video/hevc" -> profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10HDR10Plus
    "video/x-vnd.on2.vp9" -> profile in setOf(MediaCodecInfo.CodecProfileLevel.VP9Profile2HDR10Plus, MediaCodecInfo.CodecProfileLevel.VP9Profile3HDR10Plus)
    "video/av01" -> profile == MediaCodecInfo.CodecProfileLevel.AV1ProfileMain10HDR10Plus
    else -> false
}

@androidx.annotation.OptIn(UnstableApi::class)
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

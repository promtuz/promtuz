package com.promtuz.chat.utils.media

import android.content.Context
import android.media.MediaCodecInfo
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaMetadataRetriever as PlatformMetadataRetriever
import android.media.metrics.LogSessionId
import android.net.Uri
import android.os.Looper
import androidx.media3.common.C
import androidx.media3.common.ColorInfo
import androidx.media3.common.Format
import androidx.media3.common.MediaItem
import androidx.media3.common.MimeTypes
import androidx.media3.common.util.UnstableApi
import androidx.media3.container.NalUnitUtil
import androidx.media3.effect.Presentation
import androidx.media3.inspector.MetadataRetriever
import androidx.media3.transformer.AudioEncoderSettings
import androidx.media3.transformer.Codec
import androidx.media3.transformer.Composition
import androidx.media3.transformer.DefaultEncoderFactory
import androidx.media3.transformer.EditedMediaItem
import androidx.media3.transformer.Effects
import androidx.media3.transformer.EncoderSelector
import androidx.media3.transformer.EncoderUtil
import androidx.media3.transformer.ExportException
import androidx.media3.transformer.ExportResult
import androidx.media3.transformer.TransformationRequest
import androidx.media3.transformer.Transformer
import androidx.media3.transformer.VideoEncoderSettings
import com.google.common.collect.ImmutableList
import com.google.common.util.concurrent.Futures
import com.google.common.util.concurrent.ListenableFuture
import com.google.common.util.concurrent.MoreExecutors
import java.io.File
import java.io.IOException
import java.nio.file.Files
import java.util.concurrent.ExecutionException
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.math.roundToInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import uniffi.core.VideoAudioProbe
import uniffi.core.VideoEncodingPlan
import uniffi.core.VideoProbe

class VideoPreparationException(message: String, cause: Throwable? = null) : IOException(message, cause)

private val videoExports = Mutex()

/** Android media facts only. Core owns compression policy, result acceptance and candidate files. */
@androidx.annotation.OptIn(UnstableApi::class)
suspend fun inspectVideo(context: Context, file: File): VideoProbe {
    try {
        return MetadataRetriever.Builder(context.applicationContext, MediaItem.fromUri(Uri.fromFile(file)))
            .build().use { retriever ->
                val groupsFuture = retriever.retrieveTrackGroups()
                val durationFuture = retriever.retrieveDurationUs()
                try {
                    val groups = groupsFuture.awaitVideoMetadata()
                    val durationUs = durationFuture.awaitVideoMetadata()
                    val formats = buildList {
                        for (index in 0 until groups.length) {
                            val group = groups[index]
                            for (track in 0 until group.length) add(group.getFormat(track))
                        }
                    }
                    val videos = formats.filter { MimeTypes.isVideo(it.sampleMimeType) }
                    val audios = formats.filter { MimeTypes.isAudio(it.sampleMimeType) }
                    val video = videos.firstOrNull()
                        ?: throw VideoPreparationException("The video's picture track could not be read.")
                    val audio = audios.firstOrNull()
                    val platform = withContext(Dispatchers.IO) { platformVideoMetadata(file) }
                    val signal = parseVideoSignal(video, platform?.video)
                    val color = video.colorInfo
                    val transfer = color?.colorTransfer?.takeIf { it > 0 }
                        ?: platform?.video?.int(MediaFormat.KEY_COLOR_TRANSFER)?.takeIf { it > 0 }
                        ?: signal?.colorTransfer?.takeIf { it > 0 }
                    val primaries = color?.colorSpace?.takeIf { it > 0 }
                        ?: platform?.video?.int(MediaFormat.KEY_COLOR_STANDARD)?.takeIf { it > 0 }
                        ?: signal?.colorSpace?.takeIf { it > 0 }
                    val range = color?.colorRange?.takeIf { it > 0 }
                        ?: platform?.video?.int(MediaFormat.KEY_COLOR_RANGE)?.takeIf { it > 0 }
                        ?: signal?.colorRange?.takeIf { it > 0 }
                    val staticHdr = color?.hdrStaticInfo ?: platform?.video?.bytes(MediaFormat.KEY_HDR_STATIC_INFO)
                    val profile = platform?.video?.int(MediaFormat.KEY_PROFILE)
                    val mime = video.sampleMimeType ?: throw VideoPreparationException("The video format is unavailable.")
                    val hdr10Plus = hasHdr10PlusProfile(mime, profile) ||
                        platform?.video?.containsKey(MediaFormat.KEY_HDR10_PLUS_INFO) == true
                    val hdr = transfer == C.COLOR_TRANSFER_HLG || transfer == C.COLOR_TRANSFER_ST2084 ||
                        staticHdr != null || hdr10Plus || hasHdrProfile(mime, profile) || mime == MimeTypes.VIDEO_DOLBY_VISION
                    val crop = platform?.video?.cropSize()
                    // SPS dimensions apply AVC cropping / HEVC conformance windows. A container or
                    // codec's padded width/height alone need not describe the displayed picture.
                    val width = crop?.first ?: signal?.width ?: video.width
                    val height = crop?.second ?: signal?.height ?: video.height
                    val ratio = video.pixelWidthHeightRatio.toDouble().takeIf { it.isFinite() && it > 0 }
                        ?: signal?.pixelRatio ?: 1.0
                    val rotation = ((video.rotationDegrees % 360) + 360) % 360
                    if (rotation % 90 != 0) throw VideoPreparationException("The video's orientation could not be read.")
                    val pixelWidth = width * ratio
                    val depth = signal?.bitDepth ?: color?.let {
                        minOf(it.lumaBitdepth, it.chromaBitdepth).takeIf { bits -> bits > 0 }
                    }
                    VideoProbe(
                        containerMime = video.containerMimeType ?: platform?.containerMime,
                        videoMime = mime,
                        videoTracks = videos.size.toUInt(),
                        audioTracks = audios.size.toUInt(),
                        displayWidth = if (rotation % 180 == 0) pixelWidth else height.toDouble(),
                        displayHeight = if (rotation % 180 == 0) height.toDouble() else pixelWidth,
                        // Use Media3's presentation timeline, including supported edit lists, as
                        // Transformer does. Never substitute raw per-track mdhd durations.
                        durationUs = durationUs.coerceAtLeast(0).toULong(),
                        frameRate = video.frameRate.toDouble().takeIf { it.isFinite() && it > 0 }
                            ?: platform?.video?.number(MediaFormat.KEY_FRAME_RATE)?.takeIf { it.isFinite() && it > 0 },
                        videoBitrate = video.averageBitrate.takeIf { it > 0 }?.toUInt()
                            ?: platform?.video?.int(MediaFormat.KEY_BIT_RATE)?.takeIf { it > 0 }?.toUInt(),
                        audio = audio?.let {
                            VideoAudioProbe(
                                mime = it.sampleMimeType.orEmpty(),
                                // Per-track durations are not comparable after edits, gapless
                                // trimming and AAC encoder priming. Leave them unknown.
                                durationUs = null,
                                bitrate = it.averageBitrate.takeIf { bitrate -> bitrate > 0 }?.toUInt()
                                    ?: platform?.audio?.int(MediaFormat.KEY_BIT_RATE)?.takeIf { bitrate -> bitrate > 0 }?.toUInt(),
                                channels = it.channelCount.takeIf { count -> count > 0 }?.toUInt(),
                            )
                        },
                        hdr = hdr,
                        dynamicHdr = hdr10Plus || mime == MimeTypes.VIDEO_DOLBY_VISION,
                        colorPrimaries = primaries?.let(::isoPrimaries),
                        colorTransfer = transfer?.let(::isoTransfer),
                        fullRange = when (range) { C.COLOR_RANGE_FULL -> true; C.COLOR_RANGE_LIMITED -> false; else -> null },
                        bitDepth = depth?.takeIf { it in 1..255 }?.toUByte(),
                        hdrStaticMetadata = staticHdr,
                    )
                } finally {
                    groupsFuture.cancel(false)
                    durationFuture.cancel(false)
                }
            }
    } catch (error: CancellationException) {
        throw error
    } catch (error: VideoPreparationException) {
        throw error
    } catch (error: Exception) {
        throw VideoPreparationException("The video's media information could not be read.", error)
    }
}

/**
 * Executes core's plan without choosing settings, validating a result, or deleting files. All codec
 * calls and callbacks share the main looper. Cancellation releases the codec before returning so
 * core can safely clean up its source/candidate files.
 */
@androidx.annotation.OptIn(UnstableApi::class)
suspend fun encodeVideo(context: Context, source: File, output: File, input: VideoProbe, plan: VideoEncodingPlan) {
    videoExports.withLock {
        withContext(Dispatchers.IO) {
            if (source.canonicalFile == output.canonicalFile ||
                (output.exists() && Files.isSameFile(source.toPath(), output.toPath()))) {
                throw VideoPreparationException("The video encoder needs a separate output file.")
            }
        }
        val hdrColor = if (plan.keepHdr) input.hdrColorInfo()
            ?: throw VideoPreparationException("The video encoder cannot represent this HDR format.") else null
        val selector = EncoderSelector { mime ->
            val candidates = if (mime != plan.videoMime) emptyList() else if (plan.keepHdr) {
                EncoderUtil.getSupportedEncodersForHdrEditing(mime, hdrColor)
            } else EncoderUtil.getSupportedEncoders(mime)
            ImmutableList.copyOf(candidates.filter { EncoderUtil.isHardwareAccelerated(it, mime) })
        }
        val supported = withContext(Dispatchers.Default) { selector.selectEncoderInfos(plan.videoMime).isNotEmpty() }
        if (!supported) throw VideoPreparationException(if (plan.keepHdr) {
            "This device cannot encode the requested video format while preserving HDR."
        } else "This device cannot encode the requested video format.")

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
                            // KEEP_HDR itself permits automatic tone mapping. An adapter must not
                            // change the requested codec or color behavior when hardware fails.
                            if (fallbackTransformationRequest.hdrMode != Composition.HDR_MODE_KEEP_HDR ||
                                fallbackTransformationRequest.videoMimeType != plan.videoMime ||
                                fallbackTransformationRequest.audioMimeType != plan.audioMime) {
                                completion.completeExceptionally(VideoPreparationException(
                                    if (plan.keepHdr) "This device cannot encode this video while preserving HDR."
                                    else "This device cannot produce the requested video format.",
                                ))
                            }
                        }
                    })
                    .build()
                val effects = if (width != input.displayWidth.roundToInt() || height != input.displayHeight.roundToInt()) {
                    listOf(Presentation.createForWidthAndHeight(width, height, Presentation.LAYOUT_SCALE_TO_FIT))
                } else emptyList()
                val item = EditedMediaItem.Builder(MediaItem.fromUri(Uri.fromFile(source)))
                    .setFrameRate(maxFrameRate)
                    .setEffects(Effects(emptyList(), effects))
                    .build()
                // Single-item exports keep existing tracks, including no audio for silent clips.
                transformer.start(item, output.absolutePath)
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

private data class PlatformVideoMetadata(val containerMime: String?, val video: MediaFormat?, val audio: MediaFormat?)

/** Optional platform details supplement Media3 facts; platform decoder support is not required. */
private fun platformVideoMetadata(file: File): PlatformVideoMetadata? = runCatching {
    val extractor = MediaExtractor()
    var video: MediaFormat? = null
    var audio: MediaFormat? = null
    try {
        extractor.setDataSource(file.absolutePath)
        for (index in 0 until extractor.trackCount) {
            val format = extractor.getTrackFormat(index)
            val mime = format.getString(MediaFormat.KEY_MIME).orEmpty()
            if (video == null && mime.startsWith("video/")) video = format
            if (audio == null && mime.startsWith("audio/")) audio = format
        }
    } finally {
        extractor.release()
    }
    val containerMime = runCatching {
        val retriever = PlatformMetadataRetriever()
        try {
            retriever.setDataSource(file.absolutePath)
            retriever.extractMetadata(PlatformMetadataRetriever.METADATA_KEY_MIMETYPE)
        } finally { retriever.release() }
    }.getOrNull()
    PlatformVideoMetadata(containerMime, video, audio)
}.getOrNull()

private data class VideoSignal(val width: Int, val height: Int, val bitDepth: Int, val colorSpace: Int,
    val colorRange: Int, val colorTransfer: Int, val pixelRatio: Double)

@androidx.annotation.OptIn(UnstableApi::class)
private fun parseVideoSignal(format: Format, platform: MediaFormat?): VideoSignal? {
    val mime = format.sampleMimeType
    if (mime != MimeTypes.VIDEO_H264 && mime != MimeTypes.VIDEO_H265) return null
    val buffers = format.initializationData.ifEmpty { listOfNotNull(platform?.bytes("csd-0")) }
    for (data in buffers) {
        if (data.size !in 1..1_048_576) continue
        val signal = runCatching {
            val flags = BooleanArray(3)
            var offset = NalUnitUtil.findNalUnit(data, 0, data.size, flags)
            while (offset + 3 < data.size) {
                flags.fill(false)
                val end = NalUnitUtil.findNalUnit(data, offset + 3, data.size, flags)
                if (mime == MimeTypes.VIDEO_H265 && NalUnitUtil.getH265NalUnitType(data, offset) == 33) {
                    val sps = NalUnitUtil.parseH265SpsNalUnit(data, offset + 3, end, null)
                    return@runCatching VideoSignal(sps.width, sps.height,
                        8 + minOf(sps.bitDepthLumaMinus8, sps.bitDepthChromaMinus8),
                        sps.colorSpace, sps.colorRange, sps.colorTransfer, sps.pixelWidthHeightRatio.toDouble())
                }
                if (mime == MimeTypes.VIDEO_H264 && (data[offset + 3].toInt() and 31) == 7) {
                    val sps = NalUnitUtil.parseSpsNalUnit(data, offset + 3, end)
                    return@runCatching VideoSignal(sps.width, sps.height,
                        8 + minOf(sps.bitDepthLumaMinus8, sps.bitDepthChromaMinus8),
                        sps.colorSpace, sps.colorRange, sps.colorTransfer, sps.pixelWidthHeightRatio.toDouble())
                }
                offset = end
            }
            null
        }.getOrNull()
        if (signal != null) return signal
    }
    return null
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
private fun VideoProbe.hdrColorInfo(): ColorInfo? {
    val transfer = colorTransfer?.toInt()?.let(ColorInfo::isoTransferCharacteristicsToColorTransfer)
    val space = colorPrimaries?.toInt()?.let(ColorInfo::isoColorPrimariesToColorSpace)
    if (transfer !in setOf(C.COLOR_TRANSFER_HLG, C.COLOR_TRANSFER_ST2084) || space == null || space <= 0) return null
    return ColorInfo.Builder().setColorTransfer(transfer!!).setColorSpace(space)
        .setColorRange(if (fullRange == true) C.COLOR_RANGE_FULL else C.COLOR_RANGE_LIMITED)
        .setHdrStaticInfo(hdrStaticMetadata).build()
}

@androidx.annotation.OptIn(UnstableApi::class)
private fun hasHdr10PlusProfile(mime: String, profile: Int?): Boolean = when (mime) {
    MimeTypes.VIDEO_H265 -> profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10HDR10Plus
    MimeTypes.VIDEO_VP9 -> profile in setOf(MediaCodecInfo.CodecProfileLevel.VP9Profile2HDR10Plus, MediaCodecInfo.CodecProfileLevel.VP9Profile3HDR10Plus)
    MimeTypes.VIDEO_AV1 -> profile == MediaCodecInfo.CodecProfileLevel.AV1ProfileMain10HDR10Plus
    else -> false
}

@androidx.annotation.OptIn(UnstableApi::class)
private fun hasHdrProfile(mime: String, profile: Int?): Boolean = when (mime) {
    MimeTypes.VIDEO_H265 -> profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10HDR10
    MimeTypes.VIDEO_VP9 -> profile in setOf(MediaCodecInfo.CodecProfileLevel.VP9Profile2HDR, MediaCodecInfo.CodecProfileLevel.VP9Profile3HDR)
    MimeTypes.VIDEO_AV1 -> profile == MediaCodecInfo.CodecProfileLevel.AV1ProfileMain10HDR10
    else -> false
}

private suspend fun <T> ListenableFuture<T>.awaitVideoMetadata(): T = suspendCancellableCoroutine { continuation ->
    continuation.invokeOnCancellation { cancel(false) }
    addListener({
        // A ListenableFuture listener runs only after completion; getDone never blocks.
        try { continuation.resume(Futures.getDone(this)) }
        catch (error: ExecutionException) { continuation.resumeWithException(error.cause ?: error) }
        catch (error: CancellationException) { continuation.cancel(error) }
        catch (error: Exception) { continuation.resumeWithException(error) }
    }, MoreExecutors.directExecutor())
}

private fun UInt.codecInt(): Int {
    if (this == 0u || this > Int.MAX_VALUE.toUInt()) throw VideoPreparationException("The video encoder cannot represent the requested settings.")
    return toInt()
}

private fun MediaFormat.int(key: String): Int? = runCatching { if (containsKey(key)) getInteger(key) else null }.getOrNull()
private fun MediaFormat.number(key: String): Double? = runCatching { if (containsKey(key)) getFloat(key).toDouble() else null }
    .getOrElse { int(key)?.toDouble() }
private fun MediaFormat.bytes(key: String): ByteArray? = runCatching {
    val buffer = if (containsKey(key)) getByteBuffer(key)?.duplicate() else null
    if (buffer == null || buffer.remaining() !in 1..1_048_576) null else ByteArray(buffer.remaining()).also { buffer.get(it) }
}.getOrNull()

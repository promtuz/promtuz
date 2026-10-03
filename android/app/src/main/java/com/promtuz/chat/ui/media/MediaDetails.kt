package com.promtuz.chat.ui.media

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.ExifInterface
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaMetadataRetriever
import android.os.Build
import android.text.format.Formatter
import androidx.compose.ui.graphics.asAndroidBitmap
import com.promtuz.chat.ui.text.clock
import com.promtuz.chat.utils.media.MAX_ENCODED_IMAGE_BYTES
import com.promtuz.chat.utils.media.bitmapHasHdr
import com.promtuz.chat.utils.media.imageDecodeWork
import com.promtuz.chat.utils.media.prepareEncodedImage
import com.promtuz.chat.utils.media.prepareLargeImageFile
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import uniffi.core.AvifInfoRecord
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.text.DecimalFormat

internal data class MediaInfoSection(val title: String?, val rows: List<Pair<String, String>>)

internal fun mediaSummary(context: Context, item: MediaItem): List<Pair<String, String>> = buildList {
    if (item.title.isNotBlank()) add("From" to item.title)
    if (item.subtitle.isNotBlank()) add("Date" to item.subtitle)
    if (item.filePath != null) add("Name" to item.shareName)
    add("Type" to item.mime)
    (item.byteSize ?: item.encoded?.size?.toLong())?.let {
        add("Size" to Formatter.formatShortFileSize(context, it))
    }
    if (item.filePath == null && item.width > 0 && item.height > 0) {
        add("Dimensions" to "${item.width} × ${item.height}")
    }
}

/** Inspect authoritative bytes on a worker. A preview's pixel format is never the source bit depth. */
internal suspend fun readMediaDetails(context: Context, item: MediaItem): List<MediaInfoSection> = withContext(Dispatchers.IO) {
    val summary = mediaSummary(context, item).toMap().toMutableMap()
    val sections = mutableListOf<MediaInfoSection>()
    val file = item.filePath?.let(::File)?.takeIf { it.isFile }
    file?.length()?.let { summary["Size"] = Formatter.formatShortFileSize(context, it) }
    fun dimensions(width: Int, height: Int) {
        if (width > 0 && height > 0) summary["Dimensions"] = "$width × $height"
    }

    if (item.videoPath != null) {
        readVideoDetails(item.videoPath, summary, sections, ::dimensions)
    } else {
        val bytes = item.encoded ?: file?.takeIf { it.length() <= MAX_ENCODED_IMAGE_BYTES }
            ?.let { attempt { it.inputStream().use(::readBoundedImage) } }
        ensureActive()
        val avif = bytes?.let { attempt { CoreBridge.inspectAvif(it) } }
        if (avif != null) {
            dimensions(avif.width.toInt(), avif.height.toInt())
            addAvifDetails(avif, sections)
        } else {
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            attempt {
                if (bytes != null) BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
                else if (file != null) BitmapFactory.decodeFile(file.absolutePath, bounds)
            }
            dimensions(bounds.outWidth, bounds.outHeight)
            val imageRows = mutableListOf<Pair<String, String>>()
            val prefix = bytes?.take(32)?.toByteArray() ?: file?.let { attempt { it.inputStream().use { input ->
                ByteArray(32).let { buffer -> buffer.copyOf(input.read(buffer).coerceAtLeast(0)) }
            } } }
            if (prefix != null && prefix.size > 24 && prefix.copyOfRange(0, 8).contentEquals(byteArrayOf(-119, 80, 78, 71, 13, 10, 26, 10))) {
                imageRows += "Bit depth" to "${prefix[24].toInt() and 255}-bit"
            }
            if (imageRows.isNotEmpty()) sections += MediaInfoSection("Image", imageRows)
            val camera = attempt {
                val input = bytes?.let(::ByteArrayInputStream) ?: file?.inputStream()
                input?.use { cameraDetails(ExifInterface(it)) }
            }.orEmpty()
            if (camera.isNotEmpty()) sections += MediaInfoSection("Camera", camera)
        }
        ensureActive()
        // A tiny, independently owned decode reports the same profile/gain-map path as the
        // viewer without retaining another full-size bitmap or inspecting a scaled chat thumb.
        val prepared = imageDecodeWork.withPermit {
            if (bytes != null) prepareEncodedImage(bytes, targetEdge = 64, animation = false)
            else file?.let { prepareLargeImageFile(it, 64) }
        }
        prepared?.poster?.asAndroidBitmap()?.let { bitmap ->
            try {
                if (Build.VERSION.SDK_INT >= 34 && bitmap.hasGainmap()) {
                    val gainmap = bitmap.gainmap!!
                    sections += MediaInfoSection("Decoded gain map", listOf(
                        "Gain range (RGB)" to "${components(gainmap.ratioMin)} → ${components(gainmap.ratioMax)}",
                        "Gamma (RGB)" to components(gainmap.gamma),
                        "SDR offset (RGB)" to components(gainmap.epsilonSdr),
                        "HDR offset (RGB)" to components(gainmap.epsilonHdr),
                        "HDR transition ratio" to "${number(gainmap.minDisplayRatioForHdrTransition)}×",
                        "Full HDR ratio" to "${number(gainmap.displayRatioForFullHdr)}×",
                    ))
                }
                sections += MediaInfoSection("Decoded image", buildList {
                    bitmap.colorSpace?.let { add("Color space" to it.name) }
                    add("Pixel format" to when (bitmap.config) {
                        Bitmap.Config.RGBA_F16 -> "RGBA · 16-bit float per channel"
                        Bitmap.Config.RGBA_1010102 -> "RGB · 10-bit, alpha · 2-bit"
                        Bitmap.Config.ARGB_8888 -> "RGBA · 8-bit per channel"
                        else -> bitmap.config?.name ?: "Unknown"
                    })
                    add("HDR bitmap" to if (prepared.hdr || bitmapHasHdr(bitmap)) "Yes" else "No")
                    if (Build.VERSION.SDK_INT >= 34) add("Gain map" to if (bitmap.hasGainmap()) "Available" else "None")
                })
            } finally {
                bitmap.recycle()
            }
        }
    }
    listOf(MediaInfoSection(null, summary.toList())) + sections.filter { it.rows.isNotEmpty() }
}

private fun addAvifDetails(info: AvifInfoRecord, sections: MutableList<MediaInfoSection>) {
    sections += MediaInfoSection("Image", buildList {
        add("Codec" to "AV1")
        add("Bit depth" to "${info.bitDepth}-bit")
        info.chromaSubsampling?.let { add("Chroma subsampling" to it) }
        info.hasAlpha?.let { add("Alpha channel" to if (it) "Present" else "None") }
        if (info.rotationQuarterTurns.toInt() != 0) add("Rotation" to "${info.rotationQuarterTurns.toInt() * 90}° counterclockwise")
        info.mirrorAxis?.let { add("Mirror" to if (it.toInt() == 0) "Top to bottom" else "Left to right") }
    })
    sections += MediaInfoSection("Color", buildList {
        when {
            info.hasGainMap -> add("HDR representation" to "Gain map")
            info.transferCharacteristics?.toInt() == 16 -> add("HDR representation" to "PQ (SMPTE ST 2084)")
            info.transferCharacteristics?.toInt() == 18 -> add("HDR representation" to "HLG (ARIB STD-B67)")
        }
        info.colorPrimaries?.toInt()?.let { add("Color primaries" to cicp(it, primariesName(it))) }
        info.transferCharacteristics?.toInt()?.let { add("Transfer function" to cicp(it, transferName(it))) }
        info.matrixCoefficients?.toInt()?.let { add("Matrix coefficients" to cicp(it, matrixName(it))) }
        info.fullRange?.let { add("Range" to if (it) "Full" else "Limited") }
        add("ICC profile" to if (info.hasIcc) "Present" else "Not present")
        add("Gain map" to if (info.hasGainMap) "Present" else "Not present")
    })
    info.contentLightLevel?.let { light ->
        sections += MediaInfoSection("HDR light levels", listOf(
            "MaxCLL" to light.maxContentLightLevel.toInt().let { if (it == 0) "Unspecified" else "$it nits" },
            "MaxFALL" to light.maxFrameAverageLightLevel.toInt().let { if (it == 0) "Unspecified" else "$it nits" },
        ))
    }
    info.masteringDisplay?.let { display ->
        fun xy(x: UShort, y: UShort) = "${number(x.toInt() / 50000.0, 5)}, ${number(y.toInt() / 50000.0, 5)}"
        sections += MediaInfoSection("HDR mastering display", listOf(
            "Luminance" to "${number(display.minLuminance.toDouble() / 10000, 4)}–${number(display.maxLuminance.toDouble() / 10000, 4)} nits",
            "Red (x, y)" to xy(display.redX, display.redY),
            "Green (x, y)" to xy(display.greenX, display.greenY),
            "Blue (x, y)" to xy(display.blueX, display.blueY),
            "White point (x, y)" to xy(display.whiteX, display.whiteY),
        ))
    }
    if (info.animated) sections += MediaInfoSection("Animation", buildList {
        add("Frames" to info.frameCount.toString())
        info.durationMs?.let { duration ->
            add("Duration" to "${number(duration.toDouble() / 1000)} s")
            if (duration > 0uL) add("Average frame rate" to "${number(info.frameCount.toDouble() * 1000 / duration.toDouble())} fps")
        }
    })
}

private fun cameraDetails(exif: ExifInterface): List<Pair<String, String>> = buildList {
    fun attribute(tag: String) = exif.getAttribute(tag)?.trim()?.takeIf { it.isNotEmpty() }?.take(256)
    listOf(ExifInterface.TAG_MAKE, ExifInterface.TAG_MODEL).mapNotNull(::attribute).distinct()
        .joinToString(" ").takeIf { it.isNotEmpty() }?.let { add("Camera" to it) }
    attribute(ExifInterface.TAG_DATETIME_ORIGINAL)?.let { add("Captured" to it) }
    attribute(ExifInterface.TAG_ISO_SPEED_RATINGS)?.let { add("ISO" to it) }
    exif.getAttributeDouble(ExifInterface.TAG_EXPOSURE_TIME, 0.0).takeIf { it > 0 }?.let {
        add("Exposure" to if (it < 1) "1/${number(1 / it, 0)} s" else "${number(it)} s")
    }
    exif.getAttributeDouble(ExifInterface.TAG_F_NUMBER, 0.0).takeIf { it > 0 }?.let { add("Aperture" to "ƒ/${number(it)}") }
    exif.getAttributeDouble(ExifInterface.TAG_FOCAL_LENGTH, 0.0).takeIf { it > 0 }?.let { add("Focal length" to "${number(it)} mm") }
    attribute(ExifInterface.TAG_SOFTWARE)?.let { add("Software" to it) }
}

private fun readVideoDetails(
    path: String, summary: MutableMap<String, String>, sections: MutableList<MediaInfoSection>,
    dimensions: (Int, Int) -> Unit,
) {
    val video = mutableMapOf<String, String>()
    attempt {
        val retriever = MediaMetadataRetriever()
        try {
            retriever.setDataSource(path)
            fun metadata(key: Int) = retriever.extractMetadata(key)
            dimensions(metadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)?.toIntOrNull() ?: 0,
                metadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)?.toIntOrNull() ?: 0)
            metadata(MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull()?.let { summary["Duration"] = clock(it) }
            metadata(MediaMetadataRetriever.METADATA_KEY_BITRATE)?.toLongOrNull()?.let { video["Overall bitrate"] = bitrate(it) }
            metadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_ROTATION)?.toIntOrNull()?.takeIf { it != 0 }
                ?.let { video["Rotation"] = "$it°" }
        } finally { retriever.release() }
    }
    val audio = mutableListOf<MediaInfoSection>()
    attempt {
        val extractor = MediaExtractor()
        try {
            extractor.setDataSource(path)
            for (index in 0 until extractor.trackCount.coerceAtMost(32)) {
                val format = extractor.getTrackFormat(index)
                val mime = format.getString(MediaFormat.KEY_MIME).orEmpty()
                if (mime.startsWith("video/") && "Codec" !in video) {
                    video["Codec"] = codecName(mime)
                    format.number(MediaFormat.KEY_FRAME_RATE)?.takeIf { it > 0 }?.let { video["Frame rate"] = "${number(it)} fps" }
                    format.number(MediaFormat.KEY_BIT_RATE)?.takeIf { it > 0 }?.let { video["Video bitrate"] = bitrate(it.toLong()) }
                    format.integer(MediaFormat.KEY_COLOR_STANDARD)?.let { video["Color standard"] = when (it) {
                        MediaFormat.COLOR_STANDARD_BT709 -> "BT.709"
                        MediaFormat.COLOR_STANDARD_BT601_PAL -> "BT.601 PAL"
                        MediaFormat.COLOR_STANDARD_BT601_NTSC -> "BT.601 NTSC"
                        MediaFormat.COLOR_STANDARD_BT2020 -> "BT.2020"
                        else -> "Unspecified ($it)"
                    } }
                    format.integer(MediaFormat.KEY_COLOR_TRANSFER)?.let { video["Transfer function"] = when (it) {
                        MediaFormat.COLOR_TRANSFER_LINEAR -> "Linear"
                        MediaFormat.COLOR_TRANSFER_SDR_VIDEO -> "SDR video"
                        MediaFormat.COLOR_TRANSFER_ST2084 -> "PQ (SMPTE ST 2084)"
                        MediaFormat.COLOR_TRANSFER_HLG -> "HLG (ARIB STD-B67)"
                        else -> "Unspecified ($it)"
                    } }
                    format.integer(MediaFormat.KEY_COLOR_RANGE)?.let { video["Range"] = when (it) {
                        MediaFormat.COLOR_RANGE_FULL -> "Full"
                        MediaFormat.COLOR_RANGE_LIMITED -> "Limited"
                        else -> "Unspecified ($it)"
                    } }
                    if (format.containsKey(MediaFormat.KEY_HDR_STATIC_INFO)) video["HDR static metadata"] = "Present"
                    if (format.containsKey("hdr10-plus-info")) video["HDR10+ metadata"] = "Present"
                } else if (mime.startsWith("audio/")) {
                    audio += MediaInfoSection(if (audio.isEmpty()) "Audio" else "Audio ${audio.size + 1}", buildList {
                        add("Codec" to codecName(mime))
                        format.integer(MediaFormat.KEY_CHANNEL_COUNT)?.let { add("Channels" to it.toString()) }
                        format.integer(MediaFormat.KEY_SAMPLE_RATE)?.let { add("Sample rate" to "${number(it / 1000.0)} kHz") }
                        format.number(MediaFormat.KEY_BIT_RATE)?.takeIf { it > 0 }?.let { add("Bitrate" to bitrate(it.toLong())) }
                    })
                }
            }
        } finally { extractor.release() }
    }
    if (video.isNotEmpty()) sections += MediaInfoSection("Video", video.toList())
    sections += audio
}

private fun MediaFormat.integer(key: String): Int? = attempt { if (containsKey(key)) getInteger(key) else null }
private fun MediaFormat.number(key: String): Double? = integer(key)?.toDouble()
    ?: attempt { if (containsKey(key)) getFloat(key).toDouble() else null }

private fun codecName(mime: String): String = when (mime) {
    "video/av01" -> "AV1"
    "video/avc" -> "H.264 / AVC"
    "video/hevc" -> "H.265 / HEVC"
    "video/x-vnd.on2.vp8" -> "VP8"
    "video/x-vnd.on2.vp9" -> "VP9"
    "video/dolby-vision" -> "Dolby Vision"
    "audio/mp4a-latm" -> "AAC"
    "audio/opus" -> "Opus"
    "audio/mpeg" -> "MP3"
    "audio/vorbis" -> "Vorbis"
    "audio/flac" -> "FLAC"
    else -> mime
}

private fun cicp(code: Int, name: String?) = "${name ?: "Unrecognized"} · CICP $code"
private fun primariesName(code: Int): String? = when (code) {
    1 -> "BT.709 / sRGB"; 2 -> "Unspecified"; 4 -> "BT.470 M"; 5 -> "BT.470 BG"
    6 -> "SMPTE 170M"; 7 -> "SMPTE 240M"; 8 -> "Generic film"; 9 -> "BT.2020"
    10 -> "CIE XYZ"; 11 -> "DCI-P3"; 12 -> "Display P3"; 22 -> "EBU 3213-E"; else -> null
}
private fun transferName(code: Int): String? = when (code) {
    1 -> "BT.709"; 2 -> "Unspecified"; 4 -> "Gamma 2.2"; 5 -> "Gamma 2.8"
    6 -> "SMPTE 170M"; 7 -> "SMPTE 240M"; 8 -> "Linear"; 9 -> "Logarithmic (100:1)"
    10 -> "Logarithmic (316:1)"; 11 -> "IEC 61966-2-4"; 12 -> "BT.1361"
    13 -> "sRGB"; 14 -> "BT.2020 (10-bit)"; 15 -> "BT.2020 (12-bit)"
    16 -> "PQ / SMPTE ST 2084"; 17 -> "SMPTE ST 428"; 18 -> "HLG / ARIB STD-B67"; else -> null
}
private fun matrixName(code: Int): String? = when (code) {
    0 -> "Identity / RGB"; 1 -> "BT.709"; 2 -> "Unspecified"; 4 -> "FCC"; 5, 6 -> "BT.601"
    7 -> "SMPTE 240M"; 8 -> "YCgCo"; 9 -> "BT.2020 non-constant luminance"
    10 -> "BT.2020 constant luminance"; 11 -> "SMPTE ST 2085"
    12 -> "Chromaticity-derived non-constant luminance"; 13 -> "Chromaticity-derived constant luminance"
    14 -> "ICtCp"; else -> null
}

private fun number(value: Number, decimals: Int = 3): String =
    DecimalFormat(if (decimals > 0) "0.${"#".repeat(decimals)}" else "0").format(value)
private fun components(values: FloatArray) = values.joinToString(", ") { number(it, 4) }
private fun bitrate(bits: Long) = if (bits >= 1_000_000) "${number(bits / 1_000_000.0, 2)} Mbps" else "${number(bits / 1000.0)} kbps"

private fun readBoundedImage(input: InputStream): ByteArray? {
    val output = ByteArrayOutputStream()
    val buffer = ByteArray(16 * 1024)
    while (true) {
        val count = input.read(buffer)
        if (count < 0) return output.toByteArray()
        if (output.size().toLong() + count > MAX_ENCODED_IMAGE_BYTES) return null
        output.write(buffer, 0, count)
    }
}

private inline fun <T> attempt(block: () -> T): T? = try { block() }
catch (cancel: CancellationException) { throw cancel }
catch (_: Exception) { null }

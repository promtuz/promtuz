package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.graphics.Matrix
import android.media.ExifInterface
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import java.io.File
import java.io.InputStream

data class PickedFile(val path: String, val name: String, val mime: String)

/** Core reads tightly packed R,G,B,A bytes. getPixels gives unpremultiplied 0xAARRGGBB ints with a fixed
 *  layout, unlike copyPixelsToBuffer. */
fun Bitmap.toRgba(): ByteArray {
    val src = if (config == Bitmap.Config.ARGB_8888) this else copy(Bitmap.Config.ARGB_8888, false)
    val px = try {
        IntArray(src.width * src.height).also {
            src.getPixels(it, 0, src.width, 0, 0, src.width, src.height)
        }
    } finally {
        if (src !== this) src.recycle()
    }
    val out = ByteArray(px.size * 4)
    var o = 0
    for (p in px) {
        out[o++] = (p ushr 16).toByte()
        out[o++] = (p ushr 8).toByte()
        out[o++] = p.toByte()
        out[o++] = (p ushr 24).toByte()
    }
    return out
}

suspend fun decodeDownscaled(context: Context, uri: Uri, maxEdge: Int): Bitmap? =
    withContext(Dispatchers.IO) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) runCatching {
            val src = ImageDecoder.createSource(context.contentResolver, uri)
            ImageDecoder.decodeBitmap(src) { decoder, info, _ ->
                // getPixels needs a readable (non-HARDWARE) bitmap; ImageDecoder applies EXIF itself.
                decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
                val (w, h) = fit(info.size.width, info.size.height, maxEdge)
                decoder.setTargetSize(w, h)
            }
        }.getOrNull() else decodeLegacy(context, uri, maxEdge)
    }

suspend fun videoPoster(context: Context, uri: Uri, maxEdge: Int): Pair<Bitmap, Long>? =
    withContext(Dispatchers.IO) {
        runCatching {
            val r = android.media.MediaMetadataRetriever()
            try {
                r.setDataSource(context, uri)
                val duration = r.extractMetadata(android.media.MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull() ?: 0L
                val frame = r.getFrameAtTime(0, android.media.MediaMetadataRetriever.OPTION_CLOSEST_SYNC) ?: return@runCatching null
                val (w, h) = fit(frame.width, frame.height, maxEdge)
                val bmp = if (w == frame.width && h == frame.height) frame else
                    Bitmap.createScaledBitmap(frame, w, h, true).also { frame.recycle() }
                bmp to duration
            } finally { r.release() }
        }.getOrNull()
    }

suspend fun resolvePickedFile(context: Context, uri: Uri): PickedFile? {
    var privateCopy: File? = null
    try {
        return withContext(Dispatchers.IO) {
            val cr = context.contentResolver
            val sourceFile = uri.path?.takeIf { uri.scheme == "file" }?.let(::File)
            val name = sourceFile?.name ?: cr.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
                if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
            } ?: "file"
            val mime = cr.getType(uri)?.takeIf { it.isNotBlank() && it != "application/octet-stream" } ?: run {
                val extension = name.substringAfterLast('.', "").lowercase(java.util.Locale.ROOT)
                when (extension) {
                    "avif" -> "image/avif"
                    "heic" -> "image/heic"
                    "heif" -> "image/heif"
                    else -> MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension)
                }
            } ?: "application/octet-stream"
            val dir = File(context.cacheDir, "attachments").apply { mkdirs() }
            // The caller owns this independent copy until staging accepts it. Register the path
            // before copying so even cancellation on the IO return can clean up the partial file.
            val safeName = name.substringAfterLast('/').substringAfterLast('\\').filter { !it.isISOControl() }.take(180).ifBlank { "file" }
            val file = File(dir, "${java.util.UUID.randomUUID()}_$safeName")
            privateCopy = file
            cr.openInputStream(uri)?.use { input ->
                file.outputStream().use { output ->
                    val buffer = ByteArray(64 * 1024)
                    while (true) {
                        currentCoroutineContext().ensureActive()
                        val read = input.read(buffer)
                        if (read < 0) break
                        output.write(buffer, 0, read)
                    }
                }
            } ?: return@withContext null
            PickedFile(file.absolutePath, name, mime)
        }
    } catch (error: Throwable) {
        privateCopy?.let { withContext(NonCancellable + Dispatchers.IO) { it.delete() } }
        throw error
    }
}

private fun fit(w: Int, h: Int, maxEdge: Int): Pair<Int, Int> {
    val longest = maxOf(w, h)
    if (longest <= maxEdge) return w to h
    val s = maxEdge.toFloat() / longest
    return (w * s).toInt().coerceAtLeast(1) to (h * s).toInt().coerceAtLeast(1)
}

/** API 26–27 have no ImageDecoder: sample down with BitmapFactory, then apply EXIF by hand. */
private fun decodeLegacy(context: Context, uri: Uri, maxEdge: Int): Bitmap? {
    val cr = context.contentResolver
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    cr.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / sample > maxEdge) sample *= 2
    val opts = BitmapFactory.Options().apply { inSampleSize = sample }
    val raw = cr.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, opts) } ?: return null
    val (w, h) = fit(raw.width, raw.height, maxEdge)
    val scaled = if (w == raw.width && h == raw.height) raw else Bitmap.createScaledBitmap(raw, w, h, true)
    return cr.openInputStream(uri)?.use { rotateForExif(scaled, it) } ?: scaled
}

private fun rotateForExif(bmp: Bitmap, exif: InputStream): Bitmap {
    val degrees = when (ExifInterface(exif).getAttributeInt(ExifInterface.TAG_ORIENTATION, 1)) {
        ExifInterface.ORIENTATION_ROTATE_90 -> 90f
        ExifInterface.ORIENTATION_ROTATE_180 -> 180f
        ExifInterface.ORIENTATION_ROTATE_270 -> 270f
        else -> return bmp
    }
    return Bitmap.createBitmap(bmp, 0, 0, bmp.width, bmp.height, Matrix().apply { postRotate(degrees) }, true)
}

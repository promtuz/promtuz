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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.io.InputStream

/** A picked document resolved to a real filesystem path libcore can open, plus its metadata. */
data class PickedFile(val path: String, val name: String, val mime: String)

/**
 * libcore reads the pixel buffer as tightly-packed R,G,B,A bytes (4·w·h). getPixels hands back
 * 0xAARRGGBB ints; copyPixelsToBuffer's byte order is format/endianness-ambiguous, so unpack each
 * channel by hand in R,G,B,A order — this exact ordering is what makes decoded colours correct.
 */
fun Bitmap.toRgba(): ByteArray {
    val src = if (config == Bitmap.Config.ARGB_8888) this else copy(Bitmap.Config.ARGB_8888, false)
    val px = IntArray(src.width * src.height)
    src.getPixels(px, 0, src.width, 0, 0, src.width, src.height)
    val out = ByteArray(px.size * 4)
    var o = 0
    for (p in px) {
        out[o++] = (p ushr 16).toByte() // R
        out[o++] = (p ushr 8).toByte()  // G
        out[o++] = p.toByte()           // B
        out[o++] = (p ushr 24).toByte() // A
    }
    return out
}

/** Decode [uri] to a software bitmap whose longest edge is capped at [maxEdge], EXIF-oriented. */
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

/** Copy [uri]'s stream into cacheDir and read its display name + mime; null if unreadable. */
suspend fun resolvePickedFile(context: Context, uri: Uri): PickedFile? =
    withContext(Dispatchers.IO) {
        val cr = context.contentResolver
        val mime = cr.getType(uri) ?: "application/octet-stream"
        val name = cr.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
        } ?: "file"
        val dir = File(context.cacheDir, "attachments").apply { mkdirs() }
        // Prefix keeps the on-disk path unique so a same-named later pick can't clobber a file
        // still being streamed by an in-flight P2P transfer. ponytail: cache isn't pruned — add
        // a cleanup pass if it grows unbounded.
        val file = File(dir, "${System.nanoTime()}_$name")
        cr.openInputStream(uri)?.use { input -> file.outputStream().use { input.copyTo(it) } }
            ?: return@withContext null
        PickedFile(file.absolutePath, name, mime)
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

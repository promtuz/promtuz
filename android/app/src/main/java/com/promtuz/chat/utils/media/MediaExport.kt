package com.promtuz.chat.utils.media

import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.withContext
import java.io.File
import java.io.OutputStream

private const val ALBUM = "Promtuz"

suspend fun saveToGallery(context: Context, image: ImageBitmap, name: String): Boolean =
    writeToGallery(context, "image/jpeg", "$name.jpg") {
        image.asAndroidBitmap().compress(Bitmap.CompressFormat.JPEG, 92, it)
    }

/** Copy encoded media verbatim, including every animation frame and its HDR representation. */
suspend fun saveEncodedImageToGallery(context: Context, bytes: ByteArray, mime: String, name: String): Boolean =
    writeToGallery(context, mime, exportName(name, mime)) { it.write(bytes); true }

suspend fun saveFileToGallery(context: Context, path: String, mime: String, name: String): Boolean =
    writeToGallery(context, mime, exportName(name, mime)) { out ->
        File(path).inputStream().use { it.copyTo(out) }
        true
    }

private suspend fun writeToGallery(
    context: Context, mime: String, name: String, write: (OutputStream) -> Boolean,
): Boolean =
    withContext(Dispatchers.IO) {
        val video = mime.startsWith("video/")
        val collection = if (video) MediaStore.Video.Media.EXTERNAL_CONTENT_URI else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        val dir = if (video) Environment.DIRECTORY_MOVIES else Environment.DIRECTORY_PICTURES
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                put(MediaStore.MediaColumns.RELATIVE_PATH, "$dir/$ALBUM")
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
        }
        val resolver = context.contentResolver
        var created: android.net.Uri? = null
        var complete = false
        try {
            val uri = resolver.insert(collection, values) ?: return@withContext false
            created = uri
            if (resolver.openOutputStream(uri)?.use(write) != true) return@withContext false
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                values.clear()
                values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                if (resolver.update(uri, values, null, null) == 0) return@withContext false
            }
            complete = true
            true
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            false
        } finally {
            if (!complete) created?.let { uri -> runCatching { resolver.delete(uri, null, null) } }
        }
    }

suspend fun shareEncodedImage(context: Context, bytes: ByteArray, mime: String, name: String) {
    val file = withContext(Dispatchers.IO) {
        File(File(context.cacheDir, "images").apply { mkdirs() }, exportName(name, mime)).also { it.writeBytes(bytes) }
    }
    shareFile(context, file, mime)
}

private fun exportName(name: String, mime: String): String {
    val base = name.substringAfterLast('/').substringAfterLast('\\')
        .filter { it >= ' ' }.trim().trim('.').ifBlank { "image" }
    if (base.substringAfterLast('.', "").isNotEmpty()) return base
    val extension = when (mime) {
        "image/avif" -> "avif"
        "image/heic" -> "heic"
        "image/heif" -> "heif"
        else -> MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: "bin"
    }
    return "$base.$extension"
}

suspend fun sharePicture(context: Context, image: ImageBitmap, name: String) {
    val file = withContext(Dispatchers.IO) {
        File(File(context.cacheDir, "images").apply { mkdirs() }, "$name.jpg").also { f ->
            f.outputStream().use { image.asAndroidBitmap().compress(Bitmap.CompressFormat.JPEG, 92, it) }
        }
    }
    shareFile(context, file, "image/jpeg")
}

fun shareFile(context: Context, file: File, mime: String) {
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    val send = Intent(Intent.ACTION_SEND).setType(mime).putExtra(Intent.EXTRA_STREAM, uri)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    context.startActivity(Intent.createChooser(send, null).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
}

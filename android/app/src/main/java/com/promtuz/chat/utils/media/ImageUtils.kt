package com.promtuz.chat.utils.media

import android.app.Application
import android.content.Context
import android.graphics.Bitmap
import android.net.Uri
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.io.FileOutputStream


class ImageUtils(private val application: Application) {
    private val context: Context get() = application.applicationContext

    suspend fun saveImageCache(bitmap: Bitmap): Uri = withContext(Dispatchers.IO) {
        val imagesDir = File(context.cacheDir, "images").apply { mkdirs() }
        val file = File(imagesDir, "shared_image.png")


        FileOutputStream(file).use {
            bitmap.compress(Bitmap.CompressFormat.PNG, 90, it)
        }

        return@withContext FileProvider.getUriForFile(
            context,
            "${context.packageName}.fileprovider",
            file
        )
    }
}
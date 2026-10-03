package com.promtuz.chat.security

import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import androidx.annotation.RequiresApi
import timber.log.Timber
import uniffi.core.CoreException
import uniffi.core.SecureStore
import java.security.KeyStore
import java.security.ProviderException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory
import javax.crypto.spec.GCMParameterSpec

/** Blob layout: `[iv:12][ciphertext+tag:16]`. No setUnlockedDeviceRequired, because core opens the
 *  sealed secret on background threads while the device is locked. */
object KeyManager : SecureStore {
    private const val KEY_ALIAS = "master_key"
    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"

    private val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    @Synchronized
    private fun getOrCreateKey(): SecretKey {
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val key = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) strongBoxOrTee() else newKey(strongBox = false)
        logSecurityLevel(key)
        return key
    }

    @RequiresApi(Build.VERSION_CODES.P)
    private fun strongBoxOrTee(): SecretKey = try {
        newKey(strongBox = true)
    } catch (_: StrongBoxUnavailableException) {
        newKey(strongBox = false)
    } catch (_: ProviderException) {
        newKey(strongBox = false)
    }

    private fun newKey(strongBox: Boolean): SecretKey {
        val spec = KeyGenParameterSpec.Builder(
            KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        ).apply {
            setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            setKeySize(256)
            setUserAuthenticationRequired(false)
            if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) setIsStrongBoxBacked(true)
        }.build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
            .apply { init(spec) }.generateKey()
    }

    private fun logSecurityLevel(key: SecretKey) {
        runCatching {
            val info = SecretKeyFactory.getInstance(key.algorithm, ANDROID_KEYSTORE)
                .getKeySpec(key, KeyInfo::class.java) as KeyInfo
            Timber.tag("KeyManager").i("identity key securityLevel=${info.securityLevel}")
        }
    }

    override fun seal(plaintext: ByteArray): ByteArray = try {
        val cipher = Cipher.getInstance(TRANSFORMATION).apply {
            init(Cipher.ENCRYPT_MODE, getOrCreateKey())
        }
        cipher.iv + cipher.doFinal(plaintext)
    } catch (e: Exception) {
        throw CoreException.Internal("seal: ${e.message}")
    }

    override fun open(ciphertext: ByteArray): ByteArray = try {
        val iv = ciphertext.copyOfRange(0, 12)
        val body = ciphertext.copyOfRange(12, ciphertext.size)
        val cipher = Cipher.getInstance(TRANSFORMATION).apply {
            init(Cipher.DECRYPT_MODE, getOrCreateKey(), GCMParameterSpec(128, iv))
        }
        cipher.doFinal(body)
    } catch (e: Exception) {
        throw CoreException.Internal("open: ${e.message}")
    }
}

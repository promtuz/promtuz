package com.promtuz.chat.di

import com.promtuz.chat.security.KeyManager
import com.promtuz.chat.utils.media.ImageUtils
import com.promtuz.core.API
import org.koin.core.module.dsl.singleOf
import org.koin.dsl.module

val appModule = module {
    single { API }
    single { KeyManager }
    singleOf(::ImageUtils)
}
import { Module } from '@nestjs/common';
import { AppController } from './app.controller.js';
import { AppService } from './app.service.js';
import { AuthModule } from './auth/auth.module.js';
import { AuthService } from './src/auth/auth/auth.service.js';

@Module({
  imports: [AuthModule],
  controllers: [AppController],
  providers: [AppService, AuthService],
})
export class AppModule {}

import fs from 'fs/promises';
import path from 'path';
import { toHex, type Hash, type Store } from '@hashtree/core';

export class FileStore implements Store {
  private ready: Promise<void>;

  constructor(private rootDir: string) {
    this.ready = fs.mkdir(rootDir, { recursive: true });
  }

  private async filePath(hash: Hash): Promise<string> {
    await this.ready;
    return path.join(this.rootDir, toHex(hash));
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const filePath = await this.filePath(hash);
    try {
      await fs.writeFile(filePath, data, { flag: 'wx' });
      return true;
    } catch (error: any) {
      if (error?.code === 'EEXIST') {
        return false;
      }
      throw error;
    }
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const filePath = await this.filePath(hash);
    try {
      const data = await fs.readFile(filePath);
      return new Uint8Array(data);
    } catch (error: any) {
      if (error?.code === 'ENOENT') {
        return null;
      }
      throw error;
    }
  }

  async has(hash: Hash): Promise<boolean> {
    const filePath = await this.filePath(hash);
    try {
      await fs.access(filePath);
      return true;
    } catch (error: any) {
      if (error?.code === 'ENOENT') {
        return false;
      }
      throw error;
    }
  }

  async delete(hash: Hash): Promise<boolean> {
    const filePath = await this.filePath(hash);
    try {
      await fs.unlink(filePath);
      return true;
    } catch (error: any) {
      if (error?.code === 'ENOENT') {
        return false;
      }
      throw error;
    }
  }
}

export class InMemoryStore implements Store {
  private data = new Map<string, Uint8Array>();

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const key = toHex(hash);
    if (this.data.has(key)) {
      return false;
    }
    this.data.set(key, new Uint8Array(data));
    return true;
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const data = this.data.get(toHex(hash));
    if (!data) return null;
    return new Uint8Array(data);
  }

  async has(hash: Hash): Promise<boolean> {
    return this.data.has(toHex(hash));
  }

  async delete(hash: Hash): Promise<boolean> {
    return this.data.delete(toHex(hash));
  }
}

-- 给 rates 表添加 notes 字段（用户自定义备注）
ALTER TABLE rates ADD COLUMN notes TEXT NOT NULL DEFAULT '';

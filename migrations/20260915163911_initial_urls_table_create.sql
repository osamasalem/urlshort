-- Add migration script here

CREATE TABLE public.links
(
    "token" text NOT NULL,
    "url" text NOT NULL,
    "created_at" timestamp without time zone NOT NULL DEFAULT NOW(),
    CONSTRAINT pk_token PRIMARY KEY (token),
    CONSTRAINT url_unique UNIQUE (url)
);

ALTER TABLE IF EXISTS public.links
    OWNER to dbuser;

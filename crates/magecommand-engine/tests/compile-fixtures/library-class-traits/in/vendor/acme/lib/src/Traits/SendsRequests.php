<?php
namespace Acme\Lib\Traits;

trait SendsRequests
{
    public function send(string $request)
    {
        return $request;
    }

    protected function prepare(): void
    {
    }
}
